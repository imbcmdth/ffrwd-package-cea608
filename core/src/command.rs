//! The live protocol: one JSON object a line, each a command.
//!
//! - `{"say": "WORDS"}` adds text to the row on screen exactly as written,
//!   so the space between two says is the sender's: `{"say": " MORE"}`. A
//!   say that starts without one continues the word the row ends with.
//! - `{"break": true}` starts a new row for the text that follows.
//! - `{"clear": true}` erases the captions on screen.
//!
//! One object may carry several: they apply as clear, then break, then
//! say, which is the order a runner starting a fresh speaker means. A line
//! may also be an array of objects. Fields it does not know are ignored, so
//! a runner can stamp its lines with whatever it logs by.

use serde::Deserialize;
use serde_json::Value;

/// One thing to do to the captions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Say(String),
    Break,
    Clear,
}

#[derive(Deserialize)]
struct Line {
    #[serde(default)]
    say: Option<String>,
    #[serde(default, rename = "break")]
    line_break: bool,
    #[serde(default)]
    clear: bool,
}

/// The commands on one line, or why there are none.
pub fn parse(line: &str) -> Result<Vec<Command>, String> {
    let value: Value = serde_json::from_str(line).map_err(|err| format!("not JSON: {err}"))?;
    let mut out = Vec::new();
    match value {
        Value::Array(items) => {
            for item in items {
                commands(item, &mut out)?;
            }
        }
        other => commands(other, &mut out)?,
    }
    Ok(out)
}

fn commands(value: Value, out: &mut Vec<Command>) -> Result<(), String> {
    if !value.is_object() {
        return Err(format!("a command is an object, not {value}"));
    }
    let line: Line =
        serde_json::from_value(value).map_err(|err| format!("not a command: {err}"))?;
    let before = out.len();
    if line.clear {
        out.push(Command::Clear);
    }
    if line.line_break {
        out.push(Command::Break);
    }
    if let Some(text) = line.say {
        out.push(Command::Say(text));
    }
    if out.len() == before {
        return Err("a command says one of `say`, `break` or `clear`".to_owned());
    }
    Ok(())
}

/// Complete lines out of what a socket has handed over so far. What follows
/// the last newline stays in `buffer` for the next read.
pub fn lines(buffer: &mut Vec<u8>) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(at) = buffer.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buffer.drain(..=at).collect();
        let text = String::from_utf8_lossy(&line[..line.len() - 1]);
        let text = text.trim();
        if !text.is_empty() {
            out.push(text.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_command_alone() {
        assert_eq!(
            parse(r#"{"say": ">> HELLO"}"#),
            Ok(vec![Command::Say(">> HELLO".into())])
        );
        assert_eq!(parse(r#"{"break": true}"#), Ok(vec![Command::Break]));
        assert_eq!(parse(r#"{"clear": true}"#), Ok(vec![Command::Clear]));
    }

    #[test]
    fn several_in_one_object_apply_clear_break_say() {
        assert_eq!(
            parse(r#"{"say": "<< NO", "break": true, "clear": true, "at": 12.5}"#),
            Ok(vec![
                Command::Clear,
                Command::Break,
                Command::Say("<< NO".into())
            ])
        );
    }

    #[test]
    fn an_array_is_its_commands_in_order() {
        assert_eq!(
            parse(r#"[{"say": "A"}, {"break": true}, {"say": "B"}]"#),
            Ok(vec![
                Command::Say("A".into()),
                Command::Break,
                Command::Say("B".into())
            ])
        );
    }

    #[test]
    fn false_flags_and_empty_objects_say_nothing() {
        assert!(parse(r#"{"break": false}"#).is_err());
        assert!(parse("{}").is_err());
        assert!(parse("[1]").is_err());
        assert!(parse("say hi").is_err());
        assert!(parse(r#"{"say": 3}"#).is_err());
    }

    #[test]
    fn lines_split_on_newlines_and_keep_the_rest() {
        let mut buffer = b"{\"say\":\"A\"}\r\n\n{\"say\":\"B\"}\n{\"sa".to_vec();
        assert_eq!(lines(&mut buffer), vec![r#"{"say":"A"}"#, r#"{"say":"B"}"#]);
        assert_eq!(buffer, b"{\"sa");
    }
}
