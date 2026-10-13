use crate::OscMessage;

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Action {
    Shutdown,
    ChangeTitle(String),
    /// Semantic OSC message forwarded to the application.
    Osc(OscMessage),
    #[default]
    Ignore,
}
