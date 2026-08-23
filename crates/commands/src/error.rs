use thiserror::Error;

/// Parse errors carry fix-it hints: they are echoed to the command line AND
/// fed back to the LLM, so message quality is product-critical.
#[derive(Debug, Error, Clone, PartialEq)]
pub enum ParseError {
    #[error("empty command")]
    Empty,
    #[error("unknown command '{name}'{}", suggest(.suggestion))]
    UnknownCommand {
        name: String,
        suggestion: Option<String>,
    },
    #[error("'{command}' expects {expected}, got {got}. Usage: {usage}")]
    WrongArgs {
        command: &'static str,
        expected: &'static str,
        got: String,
        usage: &'static str,
    },
    #[error("'{0}' is not a number (examples: 5, 2.5, 5m, 250cm)")]
    BadNumber(String),
    #[error("'{0}' is not a point; write x,y or x,y,z (example: 0,0,0)")]
    BadPoint(String),
    #[error("'{0}' is not a selector; use 'last', 'last N', 'all', 'sel', or an object name")]
    BadSelector(String),
    #[error("'{0}' is not a color; write r,g,b with 0-1 or 0-255 values (example: 0.8,0.2,0.2)")]
    BadColor(String),
    #[error("'{0}' is not a paper size; use a4, a3, a2, a1 or a0")]
    BadPaperSize(String),
    #[error("'{0}' is not a view direction; use top, front, right or persp")]
    BadViewDirection(String),
    #[error("'{0}' is not a scale; write 1:100 or just 100")]
    BadScale(String),
}

fn suggest(s: &Option<String>) -> String {
    match s {
        Some(name) => format!(". Did you mean '{name}'?"),
        None => String::from(". Type 'help' for the command list"),
    }
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum ExecError {
    #[error("selector matched no objects{}", if .0.is_empty() { String::new() } else { format!(" ({})", .0) })]
    EmptySelection(String),
    #[error("profile must be a single closed curve; {0}")]
    BadProfile(String),
    #[error("{0}")]
    Invalid(String),
    #[error("nothing to undo")]
    NothingToUndo,
    #[error("nothing to redo")]
    NothingToRedo,
}
