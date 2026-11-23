//! Owner-facing bridge commands.
//!
//! Commands are parsed before a message is sent to MeshCore. Only the
//! configured owner can use them (the runtime checks that before calling this
//! module), and a command is never forwarded as channel/DM text.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Status,
    Advert { flood: bool },
    Name(String),
    TxPower(i32),
    PathMode(u8),
    AutoAdd(bool),
    Channels,
    Contacts,
    ChannelCreate(String),
    ChannelKey(u8),
    ChannelShare(u8),
}

/// Parse `/mesh ...` or `!mesh ...`. Returns `None` for ordinary messages and
/// `Some(Err(...))` for a command-shaped message that is invalid.
pub fn parse(text: &str) -> Option<Result<Command, String>> {
    let trimmed = text.trim();
    let rest = trimmed
        .strip_prefix("/mesh")
        .or_else(|| trimmed.strip_prefix("!mesh"))?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let mut parts = rest.split_whitespace();
    let word = parts.next().unwrap_or("help").to_ascii_lowercase();
    let args: Vec<&str> = parts.collect();
    Some(parse_command(&word, &args))
}

fn parse_command(word: &str, args: &[&str]) -> Result<Command, String> {
    match word {
        "" | "help" => Ok(Command::Help),
        "status" => Ok(Command::Status),
        "advert" => match args {
            [] | ["flood"] => Ok(Command::Advert { flood: true }),
            ["zerohop"] | ["zero-hop"] | ["zero"] => Ok(Command::Advert { flood: false }),
            _ => Err("usage: /mesh advert [flood|zerohop]".into()),
        },
        "name" => {
            let name = args.join(" ");
            if name.is_empty() {
                Err("usage: /mesh name <name>".into())
            } else if name.len() > 32 || name.chars().any(|c| !c.is_ascii() || c.is_ascii_control())
            {
                Err("name must be 1-32 printable ASCII characters".into())
            } else {
                Ok(Command::Name(name))
            }
        }
        "txpower" | "tx-power" => {
            let value = args
                .first()
                .ok_or_else(|| "usage: /mesh txpower <0-30>".to_string())?
                .parse::<i32>()
                .map_err(|_| "txpower must be an integer 0-30".to_string())?;
            if !(0..=30).contains(&value) {
                Err("txpower must be 0-30".into())
            } else {
                Ok(Command::TxPower(value))
            }
        }
        "pathmode" | "path-mode" => {
            let value = args
                .first()
                .ok_or_else(|| "usage: /mesh pathmode <0|1|2>".to_string())?
                .parse::<u8>()
                .map_err(|_| "pathmode must be 0, 1 or 2".to_string())?;
            if value > 2 {
                Err("pathmode must be 0, 1 or 2".into())
            } else {
                Ok(Command::PathMode(value))
            }
        }
        "autoadd" | "auto-add" => match args {
            ["on"] | ["true"] | ["yes"] => Ok(Command::AutoAdd(true)),
            ["off"] | ["false"] | ["no"] => Ok(Command::AutoAdd(false)),
            _ => Err("usage: /mesh autoadd on|off".into()),
        },
        "channels" => Ok(Command::Channels),
        "contacts" => Ok(Command::Contacts),
        "channel" => match args {
            ["create", rest @ ..] => {
                let name = rest.join(" ");
                if name.is_empty() {
                    Err("usage: /mesh channel create <name>".into())
                } else if name.len() > 32
                    || name.chars().any(|c| !c.is_ascii() || c.is_ascii_control())
                {
                    Err("name must be 1-32 printable ASCII characters".into())
                } else if name.starts_with('#') {
                    Err("private channel names must not start with #".into())
                } else {
                    Ok(Command::ChannelCreate(name))
                }
            }
            ["key", index] => index
                .parse::<u8>()
                .map(Command::ChannelKey)
                .map_err(|_| "usage: /mesh channel key <index>".to_string()),
            ["share", index] => index
                .parse::<u8>()
                .map(Command::ChannelShare)
                .map_err(|_| "usage: /mesh channel share <index>".to_string()),
            _ => Err("usage: /mesh channel create <name> | key <index> | share <index>".into()),
        },
        other => Err(format!("unknown command {other}; try /mesh help")),
    }
}

pub const HELP: &str = "\
MeshCore commands (owner only):
/mesh status
/mesh advert [flood|zerohop]
/mesh name <name>
/mesh txpower <0-30>
/mesh pathmode <0|1|2>
/mesh autoadd on|off
/mesh channels
/mesh contacts
/mesh channel create <name>
/mesh channel key <index>
/mesh channel share <index>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_messages_are_not_commands() {
        assert_eq!(parse("hello world"), None);
        assert_eq!(parse("/meshcore is not a command"), None);
    }

    #[test]
    fn commands_parse_and_validate() {
        assert_eq!(parse("/mesh"), Some(Ok(Command::Help)));
        assert_eq!(
            parse("/mesh advert zerohop"),
            Some(Ok(Command::Advert { flood: false }))
        );
        assert_eq!(
            parse("/mesh name Radioman Pi"),
            Some(Ok(Command::Name("Radioman Pi".into())))
        );
        assert_eq!(parse("/mesh txpower 22"), Some(Ok(Command::TxPower(22))));
        assert_eq!(parse("/mesh pathmode 2"), Some(Ok(Command::PathMode(2))));
        assert_eq!(
            parse("/mesh autoadd off"),
            Some(Ok(Command::AutoAdd(false)))
        );
        assert!(matches!(parse("/mesh txpower 99"), Some(Err(_))));
        assert!(matches!(parse("/mesh name "), Some(Err(_))));
        assert!(matches!(parse("/mesh nope"), Some(Err(_))));
        assert_eq!(
            parse("/mesh channel create Family"),
            Some(Ok(Command::ChannelCreate("Family".into())))
        );
        assert_eq!(
            parse("/mesh channel key 2"),
            Some(Ok(Command::ChannelKey(2)))
        );
        assert_eq!(
            parse("/mesh channel share 2"),
            Some(Ok(Command::ChannelShare(2)))
        );
    }
}
