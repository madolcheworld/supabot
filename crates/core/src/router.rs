//! Group routing: who a message wakes.
//!
//! Precedence, first match wins:
//!
//! 1. explicit addressees (a reply-to in the UI, a click);
//! 2. `@mentions` in the text, where `@everyone` means all members;
//! 3. the author of the message being replied to;
//! 4. the group's lead bot.
//!
//! Two rules keep a group from turning into a pinball machine: a member is
//! woken **at most once per reply** (deduped here), and bots are woken with the
//! final message, not with mid-turn commentary (the caller's job).

use serde::{Deserialize, Serialize};

use swarmbot_contracts::bot::Bot;
use swarmbot_contracts::id::BotId;

/// The exact reply that means "I have nothing to add".
pub const NO_REPLY: &str = "NO_REPLY";

/// The mention that addresses every member.
pub const EVERYONE: &str = "everyone";

/// Why these bots were chosen. Shown in the transcript so a routing surprise is
/// explainable rather than mysterious.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteReason {
    /// Named in the UI as the addressee.
    Addressed,
    Mention,
    Everyone,
    ReplyTo,
    Lead,
    /// Nobody: a direct message to one bot does not route.
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteDecision {
    pub targets: Vec<BotId>,
    pub reason: RouteReason,
    pub handles: Vec<String>,
}

impl RouteDecision {
    fn empty() -> Self {
        Self {
            targets: Vec::new(),
            reason: RouteReason::None,
            handles: Vec::new(),
        }
    }

    pub fn names<'a>(&self, members: &'a [Bot]) -> Vec<&'a str> {
        self.targets
            .iter()
            .filter_map(|id| members.iter().find(|b| &b.id == id))
            .map(|b| b.name.as_str())
            .collect()
    }
}

pub struct GroupRouter;

impl GroupRouter {
    /// Decide which members a group message wakes.
    ///
    /// `addressed` is what the UI named explicitly; `reply_to` is the author of
    /// the message being replied to. `members` must be the group's bots.
    pub fn route(
        members: &[Bot],
        text: &str,
        addressed: &[BotId],
        reply_to: Option<&BotId>,
        lead: &BotId,
    ) -> RouteDecision {
        if !addressed.is_empty() {
            let targets = dedupe(
                addressed
                    .iter()
                    .filter(|id| members.iter().any(|m| &m.id == *id))
                    .cloned()
                    .collect(),
            );
            if !targets.is_empty() {
                return RouteDecision {
                    targets,
                    reason: RouteReason::Addressed,
                    handles: Vec::new(),
                };
            }
        }

        let mut handles = mentioned_handles(text);
        if handles.iter().any(|h| h == EVERYONE) || contains_word_ci(text, EVERYONE) {
            return RouteDecision {
                targets: members.iter().map(|m| m.id.clone()).collect(),
                reason: RouteReason::Everyone,
                handles,
            };
        }

        let mut targets: Vec<BotId> = Vec::new();

        // 1. Matches from explicit @mentions
        for handle in &handles {
            if let Some(bot) = members.iter().find(|m| m.name.eq_ignore_ascii_case(handle)) {
                targets.push(bot.id.clone());
            }
        }

        // 2. Matches from natural bot names and labels
        for member in members {
            if contains_word_ci(text, &member.name) || contains_word_ci(text, &member.label) {
                if !handles.iter().any(|h| h.eq_ignore_ascii_case(&member.name)) {
                    handles.push(member.name.clone());
                }
                targets.push(member.id.clone());
            }
        }

        if !targets.is_empty() {
            return RouteDecision {
                targets: dedupe(targets),
                reason: RouteReason::Mention,
                handles,
            };
        }

        if let Some(author) = reply_to
            && let Some(bot) = members.iter().find(|m| m.id == *author)
        {
            return RouteDecision {
                targets: vec![bot.id.clone()],
                reason: RouteReason::ReplyTo,
                handles,
            };
        }

        if let Some(lead_bot) = members.iter().find(|m| m.id == *lead) {
            return RouteDecision {
                targets: vec![lead_bot.id.clone()],
                reason: RouteReason::Lead,
                handles,
            };
        }

        // A group whose lead has left: wake the first member rather than
        // dropping the message on the floor.
        members
            .first()
            .map_or_else(RouteDecision::empty, |bot| RouteDecision {
                targets: vec![bot.id.clone()],
                reason: RouteReason::Lead,
                handles,
            })
    }

    /// Which members a bot's reply wakes: the ones it mentioned or directly addressed,
    /// minus the ones already addressed by the message it answers.
    pub fn wake_from_reply(
        members: &[Bot],
        reply_text: &str,
        already_addressed: &[BotId],
    ) -> Vec<BotId> {
        let handles = mentioned_handles(reply_text);
        let mut targets: Vec<BotId> = handles
            .iter()
            .filter_map(|handle| {
                members
                    .iter()
                    .find(|m| handle_matches_bot(handle, m))
                    .map(|m| m.id.clone())
            })
            .collect();
        for member in members {
            if is_bot_addressed_in_reply(reply_text, member) {
                targets.push(member.id.clone());
            }
        }
        if handles.iter().any(|h| h == EVERYONE) || contains_word_ci(reply_text, EVERYONE) {
            targets.extend(members.iter().map(|m| m.id.clone()));
        }
        targets.retain(|id| !already_addressed.contains(id));
        dedupe(targets)
    }
}

fn handle_matches_bot(handle: &str, bot: &Bot) -> bool {
    if bot.name.eq_ignore_ascii_case(handle) {
        return true;
    }
    let label = bot.label.trim();
    if label.is_empty() {
        return false;
    }
    if !label.chars().any(char::is_whitespace) && label.to_lowercase() == handle.to_lowercase() {
        return true;
    }
    if label.chars().any(|c| c.is_ascii_alphanumeric()) {
        let normalized = swarmbot_contracts::bot::normalize_name(label);
        if !normalized.is_empty() && normalized.eq_ignore_ascii_case(handle) {
            return true;
        }
    }
    false
}

/// Check if a bot is directly addressed in a bot's reply (via friendly greeting or vocative).
pub fn is_bot_addressed_in_reply(text: &str, bot: &Bot) -> bool {
    let name = &bot.name;
    let label = &bot.label;

    for n in [name.as_str(), label.as_str()] {
        let n = n.trim();
        if n.is_empty() {
            continue;
        }
        if contains_word_ci(text, &format!("{n},")) || contains_word_ci(text, &format!("{n}:")) {
            return true;
        }
        for greeting in ["halo", "hai", "hi", "hey", "hello", "dear"] {
            if contains_word_ci(text, &format!("{greeting} {n}")) {
                return true;
            }
        }
    }
    false
}

/// True when a bot's reply means "nothing to add", so it is not posted.
///
/// Compared after trimming, and case-insensitively: a model that answers
/// `no_reply` still meant it. Anything longer is a real reply — a sentence
/// that merely contains the token is posted, because swallowing a real answer
/// is worse than posting a redundant one.
pub fn is_no_reply(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case(NO_REPLY)
}

/// Every `@handle` in a text, lowercased, in order of first appearance,
/// deduplicated. A mention is `@` followed by letters, digits, `-` or `_`;
/// an email address does not count (the `@` must not be preceded by a word
/// character).
pub fn mentioned_handles(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let bytes: Vec<char> = text.chars().collect();
    for (index, ch) in bytes.iter().enumerate() {
        if *ch != '@' {
            continue;
        }
        if index > 0 {
            let previous = bytes[index - 1];
            if previous.is_alphanumeric() || previous == '_' || previous == '.' {
                continue;
            }
        }
        let mut handle = String::new();
        for next in bytes.iter().skip(index + 1) {
            if next.is_alphanumeric() || *next == '-' || *next == '_' {
                handle.extend(next.to_lowercase());
            } else {
                break;
            }
        }
        if handle.is_empty() {
            continue;
        }
        if !found.contains(&handle) {
            found.push(handle);
        }
    }
    found
}

/// Check if a word/name appears in `text` on word boundaries, case-insensitively.
///
/// A word boundary is start/end of string or any character other than alphanumeric,
/// `_`, or `-`. If the character immediately preceding is `@`, it only matches if
/// that `@` was itself at a word boundary (so `@lead` matches, but `user@lead.com` does not).
pub fn contains_word_ci(text: &str, word: &str) -> bool {
    if word.is_empty() || text.is_empty() {
        return false;
    }
    let word_lower = word.to_lowercase();
    let text_lower = text.to_lowercase();

    let mut search_from = 0;
    while let Some(byte_idx) = text_lower[search_from..].find(&word_lower) {
        let match_start = search_from + byte_idx;
        let match_end = match_start + word_lower.len();

        // Check boundaries against `text_lower` (never indexing `text` with
        // `text_lower` byte offsets, because case-folding can expand UTF-8 byte lengths).
        let before_ok = match text_lower[..match_start].chars().next_back() {
            None => true,
            Some('@') => {
                let before_at = &text_lower[..match_start - '@'.len_utf8()];
                before_at
                    .chars()
                    .next_back()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '.')
            }
            Some(last_char) => !last_char.is_alphanumeric() && last_char != '_' && last_char != '-',
        };

        let after_ok = match text_lower[match_end..].chars().next() {
            None => true,
            Some(next_char) => !next_char.is_alphanumeric() && next_char != '_' && next_char != '-',
        };

        if before_ok && after_ok {
            return true;
        }

        let Some(step) = text_lower[match_start..].chars().next().map(char::len_utf8) else {
            break;
        };
        search_from = match_start + step;
    }
    false
}

fn dedupe(mut ids: Vec<BotId>) -> Vec<BotId> {
    let mut seen = Vec::new();
    ids.retain(|id| {
        if seen.contains(id) {
            false
        } else {
            seen.push(id.clone());
            true
        }
    });
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    fn team() -> Vec<Bot> {
        let store = testing::memory_store();
        vec![
            testing::seed_bot(&store, "lead"),
            testing::seed_bot(&store, "scout"),
            testing::seed_bot(&store, "dev"),
        ]
    }

    fn by_name<'a>(members: &'a [Bot], name: &str) -> &'a Bot {
        members.iter().find(|b| b.name == name).unwrap()
    }

    #[test]
    fn mentions_win_over_the_lead() {
        let members = team();
        let lead = &by_name(&members, "lead").id;
        let decision = GroupRouter::route(&members, "@scout look at this", &[], None, lead);
        assert_eq!(decision.reason, RouteReason::Mention);
        assert_eq!(
            decision.targets,
            vec![by_name(&members, "scout").id.clone()]
        );
    }

    #[test]
    fn several_mentions_wake_several_bots_once_each() {
        let members = team();
        let lead = &by_name(&members, "lead").id;
        let decision = GroupRouter::route(&members, "@scout @dev @scout — go", &[], None, lead);
        assert_eq!(
            decision.targets.len(),
            2,
            "a repeated mention wakes nobody twice"
        );
        assert_eq!(decision.reason, RouteReason::Mention);
    }

    #[test]
    fn everyone_means_everyone() {
        let members = team();
        let lead = &by_name(&members, "lead").id;
        let decision = GroupRouter::route(&members, "@everyone standup", &[], None, lead);
        assert_eq!(decision.reason, RouteReason::Everyone);
        assert_eq!(decision.targets.len(), members.len());
    }

    #[test]
    fn an_explicit_addressee_beats_the_text() {
        let members = team();
        let dev = by_name(&members, "dev").id.clone();
        let lead = &by_name(&members, "lead").id;
        let decision = GroupRouter::route(
            &members,
            "@scout help",
            std::slice::from_ref(&dev),
            None,
            lead,
        );
        assert_eq!(decision.reason, RouteReason::Addressed);
        assert_eq!(decision.targets, vec![dev]);
    }

    #[test]
    fn a_reply_to_its_author_when_nobody_is_mentioned() {
        let members = team();
        let scout = by_name(&members, "scout").id.clone();
        let lead = &by_name(&members, "lead").id;
        let decision = GroupRouter::route(&members, "and?", &[], Some(&scout), lead);
        assert_eq!(decision.reason, RouteReason::ReplyTo);
        assert_eq!(decision.targets, vec![scout]);
    }

    #[test]
    fn otherwise_the_lead_answers() {
        let members = team();
        let lead = by_name(&members, "lead").id.clone();
        let decision = GroupRouter::route(&members, "what now?", &[], None, &lead);
        assert_eq!(decision.reason, RouteReason::Lead);
        assert_eq!(decision.targets, vec![lead]);
    }

    #[test]
    fn a_group_without_its_lead_falls_back_to_its_first_member() {
        let members = team();
        let outsider = BotId::new();
        let decision = GroupRouter::route(&members, "what now?", &[], None, &outsider);
        assert_eq!(decision.targets, vec![members[0].id.clone()]);
        assert_eq!(decision.reason, RouteReason::Lead);
    }

    #[test]
    fn a_mention_of_a_stranger_does_not_hijack_the_route() {
        let members = team();
        let lead = &by_name(&members, "lead").id;
        let decision = GroupRouter::route(&members, "@nobody are you there?", &[], None, lead);
        assert_eq!(decision.reason, RouteReason::Lead, "the lead still answers");
        assert!(decision.handles.contains(&"nobody".to_string()));
    }

    #[test]
    fn a_reply_wakes_the_members_it_mentions_once() {
        let members = team();
        let scout = by_name(&members, "scout").id.clone();
        let dev = by_name(&members, "dev").id.clone();
        // The message being answered already addressed scout.
        let woken = GroupRouter::wake_from_reply(
            &members,
            "@scout @dev done",
            std::slice::from_ref(&scout),
        );
        assert_eq!(woken, vec![dev]);

        let all = GroupRouter::wake_from_reply(&members, "@everyone wrap up", &[]);
        assert_eq!(all.len(), members.len());
    }

    #[test]
    fn no_reply_is_only_the_token_alone() {
        assert!(is_no_reply("NO_REPLY"));
        assert!(is_no_reply("  NO_REPLY\n"));
        assert!(is_no_reply("no_reply"));
        assert!(!is_no_reply("NO_REPLY, but check the log"));
        assert!(!is_no_reply("I have nothing to add"));
    }

    #[test]
    fn mentions_are_parsed_without_swallowing_addresses() {
        assert_eq!(
            mentioned_handles("@lead and @dev-ops"),
            vec!["lead", "dev-ops"]
        );
        assert_eq!(
            mentioned_handles("mail me at ria@example.com or @lead"),
            vec!["lead"],
            "an email address is not a mention"
        );
        assert_eq!(mentioned_handles("@Lead twice @lead"), vec!["lead"]);
        assert_eq!(mentioned_handles("@ alone"), Vec::<String>::new());
        assert_eq!(mentioned_handles("no mentions here"), Vec::<String>::new());
        assert_eq!(mentioned_handles("@everyone"), vec!["everyone"]);
    }

    #[test]
    fn a_decision_can_name_its_targets() {
        let members = team();
        let lead = &by_name(&members, "lead").id;
        let decision = GroupRouter::route(&members, "@dev go", &[], None, lead);
        assert_eq!(decision.names(&members), vec!["dev"]);
    }

    #[test]
    fn natural_greeting_and_bot_name_without_at_wakes_bot() {
        let members = team();
        let scout = by_name(&members, "scout").id.clone();
        let dev = by_name(&members, "dev").id.clone();
        let lead = &by_name(&members, "lead").id;

        // Greeting without @
        let decision =
            GroupRouter::route(&members, "halo scout, tolong cari file", &[], None, lead);
        assert_eq!(decision.reason, RouteReason::Mention);
        assert_eq!(decision.targets, vec![scout.clone()]);

        // Direct call with colon
        let decision = GroupRouter::route(&members, "dev: tolong compile", &[], None, lead);
        assert_eq!(decision.reason, RouteReason::Mention);
        assert_eq!(decision.targets, vec![dev.clone()]);

        // Multiple bots mentioned naturally
        let decision = GroupRouter::route(
            &members,
            "tolong tanyakan ke dev dan scout ya",
            &[],
            None,
            lead,
        );
        assert_eq!(decision.reason, RouteReason::Mention);
        assert_eq!(decision.targets.len(), 2);
        assert!(decision.targets.contains(&scout));
        assert!(decision.targets.contains(&dev));
    }

    #[test]
    fn word_boundary_matcher_avoids_false_positives() {
        assert!(!contains_word_ci("developer sedang bekerja", "dev"));
        assert!(!contains_word_ci("email@dev.org", "dev"));
        assert!(contains_word_ci("halo dev!", "dev"));
        assert!(contains_word_ci("dev, tolong", "dev"));
        assert!(contains_word_ci("@dev", "dev"));
        assert!(contains_word_ci("hey @dev!", "dev"));
        assert!(contains_word_ci("Dev", "dev"));
    }

    #[test]
    fn wake_from_reply_wakes_naturally_named_bots() {
        let members = team();
        let scout = by_name(&members, "scout").id.clone();
        let dev = by_name(&members, "dev").id.clone();

        let woken = GroupRouter::wake_from_reply(
            &members,
            "halo dev tolong bantu ini",
            std::slice::from_ref(&scout),
        );
        assert_eq!(woken, vec![dev]);

        let woken_vocative = GroupRouter::wake_from_reply(
            &members,
            "dev: tolong compile kode ini",
            std::slice::from_ref(&scout),
        );
        assert_eq!(woken_vocative, vec![by_name(&members, "dev").id.clone()]);

        let woken_narrative = GroupRouter::wake_from_reply(
            &members,
            "On it — handing the search to dev.",
            std::slice::from_ref(&scout),
        );
        assert!(
            woken_narrative.is_empty(),
            "third-person reference does not wake"
        );
    }

    #[test]
    fn contains_word_ci_handles_utf8_emoji_and_indonesian_without_panicking() {
        // Multi-byte leading emoji + em-dash where `+ 1` byte stepping used to panic.
        assert!(contains_word_ci("😀dev_bad — tolong cek dev!", "dev"));
        assert!(!contains_word_ci("😀dev_bad — developer bekerja", "dev"));
        // Case-folding expansion (`İ` 2 bytes -> `i\u{307}` 3 bytes) shifts byte offsets.
        assert!(contains_word_ci("İstanbul 🎉 halo dev, siap?", "dev"));
        assert!(!contains_word_ci("İdeveloper 🎉", "dev"));
        // Indonesian punctuation & multi-byte quotes/dashes around handles.
        assert!(contains_word_ci("Tolong cek ini ya “@scout” 🙏", "scout"));
        assert!(contains_word_ci("Terima kasih 🙏—dev, cek berkas.", "dev"));
        assert!(!contains_word_ci("kirim@dev.co.id", "dev"));
    }

    #[test]
    fn wake_from_reply_matches_at_label_alongside_name() {
        let store = testing::memory_store();
        let mut lead = testing::seed_bot(&store, "lead");
        lead.label = "Architect".to_string();
        let mut dev = testing::seed_bot(&store, "dev");
        dev.label = "Code Reviewer".to_string();
        let scout = testing::seed_bot(&store, "scout");
        let members = vec![lead.clone(), dev.clone(), scout.clone()];

        let woken_single = GroupRouter::wake_from_reply(
            &members,
            "@Architect please review",
            std::slice::from_ref(&scout.id),
        );
        assert_eq!(woken_single, vec![lead.id]);

        let woken_normalized = GroupRouter::wake_from_reply(
            &members,
            "Over to @code-reviewer for a check",
            &[scout.id],
        );
        assert_eq!(woken_normalized, vec![dev.id]);
    }
}
