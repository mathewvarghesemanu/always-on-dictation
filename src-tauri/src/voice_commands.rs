//! Spoken key commands: phrases like "press enter" in the final text become key
//! presses. Runs on the text about to be pasted, so after post-processing.

use crate::settings::VoiceCommand;
use enigo::{Direction, Enigo, Key, Keyboard};
use regex::Regex;

/// A piece of output: text to paste, or keys to press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    Keys(String),
}

/// Punctuation a transcriber or LLM may put around a spoken command.
const EDGE_PUNCT: &[char] = &['.', ',', '!', '?', ';', ':', '…'];

/// Matches `phrase` case-insensitively on word boundaries, letting any run of
/// spaces or punctuation stand between its words ("press, enter").
fn phrase_pattern(phrase: &str) -> Option<String> {
    let words: Vec<String> = phrase
        .split(|c: char| c.is_whitespace() || EDGE_PUNCT.contains(&c) || c == '-')
        .filter(|w| !w.is_empty())
        .map(regex::escape)
        .collect();
    if words.is_empty() {
        return None;
    }
    Some(format!(r"\b{}\b", words.join(r"[\s\p{P}]+")))
}

fn usable(command: &VoiceCommand) -> bool {
    !command.keys.trim().is_empty() && phrase_pattern(&command.phrase).is_some()
}

/// Trims the text before a command: whitespace and dangling ", ; :".
fn trim_before(text: &str) -> &str {
    text.trim_end().trim_end_matches([',', ';', ':']).trim_end()
}

/// Trims the text after a command: whitespace and punctuation the command took.
fn trim_after(text: &str) -> &str {
    text.trim_start_matches(|c: char| c.is_whitespace() || EDGE_PUNCT.contains(&c))
}

/// Splits `text` into text and key presses. Text without any command comes
/// back as one untouched `Text` segment.
pub fn split(text: &str, commands: &[VoiceCommand]) -> Vec<Segment> {
    let commands: Vec<&VoiceCommand> = commands.iter().filter(|c| usable(c)).collect();
    if commands.is_empty() {
        return vec![Segment::Text(text.to_string())];
    }

    // End-only commands: peel them off the end, repeatedly, so "press tab
    // press enter" at the end presses both.
    let end_res: Vec<(Regex, &str)> = commands
        .iter()
        .filter(|c| c.end_only)
        .filter_map(|c| {
            let pattern = phrase_pattern(&c.phrase)?;
            Regex::new(&format!(r"(?i){pattern}[\s\p{{P}}]*$"))
                .ok()
                .map(|re| (re, c.keys.as_str()))
        })
        .collect();
    let mut body = text;
    let mut tail_keys: Vec<String> = Vec::new();
    loop {
        // The longest match wins, so "press shift enter" beats "press enter"'s
        // overlap with it.
        let best = end_res
            .iter()
            .filter_map(|(re, keys)| re.find(body).map(|m| (m.start(), *keys)))
            .min_by_key(|(start, _)| *start);
        match best {
            Some((start, keys)) => {
                tail_keys.push(keys.to_string());
                body = trim_before(&body[..start]);
            }
            None => break,
        }
    }
    tail_keys.reverse();

    // Anywhere commands: every occurrence in what's left.
    let any_res: Vec<(Regex, &str)> = commands
        .iter()
        .filter(|c| !c.end_only)
        .filter_map(|c| {
            let pattern = phrase_pattern(&c.phrase)?;
            Regex::new(&format!("(?i){pattern}"))
                .ok()
                .map(|re| (re, c.keys.as_str()))
        })
        .collect();
    let mut hits: Vec<(usize, usize, &str)> = any_res
        .iter()
        .flat_map(|(re, keys)| re.find_iter(body).map(move |m| (m.start(), m.end(), *keys)))
        .collect();
    // Earliest first; at the same start, the longest.
    hits.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));

    let mut segments = Vec::new();
    let mut cursor = 0;
    for (start, end, keys) in hits {
        if start < cursor {
            continue; // Overlaps a command already taken.
        }
        let before = &body[cursor..start];
        let before = if cursor == 0 {
            trim_before(before)
        } else {
            trim_before(trim_after(before))
        };
        if !before.is_empty() {
            segments.push(Segment::Text(before.to_string()));
        }
        segments.push(Segment::Keys(keys.to_string()));
        cursor = end;
    }

    if cursor == 0 && tail_keys.is_empty() {
        return vec![Segment::Text(text.to_string())];
    }

    let rest = if cursor == 0 {
        body
    } else {
        trim_after(&body[cursor..])
    };
    // Only the start is trimmed, so text that ends the output keeps its own
    // end (e.g. a trailing space); text before end keys is already trimmed.
    if !rest.is_empty() {
        segments.push(Segment::Text(rest.to_string()));
    }
    segments.extend(tail_keys.into_iter().map(Segment::Keys));
    segments
}

/// Maps a key token as the key recorder writes it ("ctrl", "enter", "a").
fn parse_key(token: &str) -> Result<Key, String> {
    let key = match token {
        "ctrl" | "control" => Key::Control,
        "shift" => Key::Shift,
        "alt" | "option" => Key::Alt,
        "command" | "cmd" | "meta" | "super" | "win" | "windows" => Key::Meta,
        "enter" | "return" => Key::Return,
        "tab" => Key::Tab,
        "space" => Key::Space,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "esc" | "escape" => Key::Escape,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "capslock" => Key::CapsLock,
        #[cfg(not(target_os = "macos"))]
        "insert" => Key::Insert,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        "f13" => Key::F13,
        "f14" => Key::F14,
        "f15" => Key::F15,
        "f16" => Key::F16,
        "f17" => Key::F17,
        "f18" => Key::F18,
        "f19" => Key::F19,
        "f20" => Key::F20,
        other => {
            let mut chars = other.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Key::Unicode(c),
                _ => return Err(format!("Unsupported key '{other}'")),
            }
        }
    };
    Ok(key)
}

/// Parses "ctrl+shift+enter" into keys, modifiers first as recorded.
pub fn parse_combo(combo: &str) -> Result<Vec<Key>, String> {
    let keys = combo
        .split('+')
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        // The recorder may write sided modifiers ("shift_left").
        .map(|t| {
            let base = t
                .strip_suffix("_left")
                .or_else(|| t.strip_suffix("_right"))
                .unwrap_or(&t)
                .to_string();
            parse_key(&base)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if keys.is_empty() {
        return Err("Empty key combination".into());
    }
    Ok(keys)
}

/// Presses every key of `combo` in order, then releases them in reverse.
pub fn press_combo(enigo: &mut Enigo, combo: &str) -> Result<(), String> {
    let keys = parse_combo(combo)?;
    let mut pressed = Vec::new();
    let mut result = Ok(());
    for key in &keys {
        if let Err(e) = enigo.key(*key, Direction::Press) {
            result = Err(format!("Failed to press {key:?}: {e}"));
            break;
        }
        pressed.push(*key);
    }
    // Release whatever went down, even after a failure, so nothing sticks.
    for key in pressed.iter().rev() {
        if let Err(e) = enigo.key(*key, Direction::Release) {
            if result.is_ok() {
                result = Err(format!("Failed to release {key:?}: {e}"));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(phrase: &str, keys: &str, end_only: bool) -> VoiceCommand {
        VoiceCommand {
            id: phrase.into(),
            phrase: phrase.into(),
            keys: keys.into(),
            end_only,
        }
    }

    fn text(s: &str) -> Segment {
        Segment::Text(s.into())
    }

    fn keys(s: &str) -> Segment {
        Segment::Keys(s.into())
    }

    #[test]
    fn no_commands_leaves_text_untouched() {
        assert_eq!(split(" hi there. ", &[]), vec![text(" hi there. ")]);
        let c = [cmd("press enter", "enter", true)];
        assert_eq!(split("hi there ", &c), vec![text("hi there ")]);
    }

    #[test]
    fn end_only_matches_at_end_with_punctuation() {
        let c = [cmd("press enter", "enter", true)];
        assert_eq!(
            split("Send the report, press enter.", &c),
            vec![text("Send the report"), keys("enter")]
        );
        assert_eq!(split("Press Enter!", &c), vec![keys("enter")]);
        assert_eq!(split("Press, enter. ", &c), vec![keys("enter")]);
    }

    #[test]
    fn end_only_ignores_middle() {
        let c = [cmd("press enter", "enter", true)];
        let t = "If you press enter it sends.";
        assert_eq!(split(t, &c), vec![text(t)]);
    }

    #[test]
    fn end_only_chains() {
        let c = [
            cmd("press enter", "enter", true),
            cmd("press tab", "tab", true),
        ];
        assert_eq!(
            split("hello press tab press enter", &c),
            vec![text("hello"), keys("tab"), keys("enter")]
        );
    }

    #[test]
    fn anywhere_splits_text() {
        let c = [cmd("new line", "shift+enter", false)];
        assert_eq!(
            split("First. New line. Second ", &c),
            vec![text("First."), keys("shift+enter"), text("Second ")]
        );
    }

    #[test]
    fn anywhere_respects_word_boundaries() {
        let c = [cmd("tab", "tab", false)];
        assert_eq!(split("a table", &c), vec![text("a table")]);
    }

    #[test]
    fn longer_phrase_wins_overlap() {
        let c = [
            cmd("press enter", "enter", false),
            cmd("press enter twice", "enter+enter", false),
        ];
        assert_eq!(
            split("ok press enter twice", &c),
            vec![text("ok"), keys("enter+enter")]
        );
    }

    #[test]
    fn mixed_anywhere_and_end() {
        let c = [
            cmd("press tab", "tab", false),
            cmd("press enter", "enter", true),
        ];
        assert_eq!(
            split("name press tab email press enter", &c),
            vec![text("name"), keys("tab"), text("email"), keys("enter")]
        );
    }

    #[test]
    fn parses_combos() {
        assert_eq!(
            parse_combo("ctrl+shift+enter").unwrap(),
            vec![Key::Control, Key::Shift, Key::Return]
        );
        assert_eq!(
            parse_combo("command+k").unwrap(),
            vec![Key::Meta, Key::Unicode('k')]
        );
        assert_eq!(
            parse_combo("shift_left+tab").unwrap(),
            vec![Key::Shift, Key::Tab]
        );
        assert!(parse_combo("bogus").is_err());
        assert!(parse_combo("").is_err());
    }

    #[test]
    fn skips_unusable_commands() {
        let c = [cmd("  ", "enter", true), cmd("go", "", true)];
        assert_eq!(split("go", &c), vec![text("go")]);
    }
}
