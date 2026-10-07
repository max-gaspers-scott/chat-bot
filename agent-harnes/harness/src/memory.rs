//! Memory trait and in-memory implementation.
//!
//! We re-export rig's `Message` directly as our conversation message type —
//! it already has everything we need (user/assistant/system, tool calls,
//! tool results).  The `Memory` trait gives us a stable boundary so that
//! later phases can add DB persistence or compaction behind the same interface.



// Re-export rig's message types so the rest of the crate only imports from us.
pub use rig_core::completion::message::{
    AssistantContent, Message, Text, ToolCall, ToolFunction, ToolResult, ToolResultContent,
    UserContent,
};

// ── Memory trait ─────────────────────────────────────────────────────────────

/// A read/append/replace store for conversation history.
///
/// Implementations must be `Send + Sync` so they can be shared across async
/// tasks.  The in-memory implementation is just a `Vec<Message>`; future
/// implementations may persist to SQLite or another store.
pub trait Memory: Send + Sync {
    /// Return a snapshot of the current history (oldest first).
    fn load(&self) -> Vec<Message>;

    /// Append one message to the end of the history.
    fn append(&mut self, message: Message);

    /// Replace the entire history with `messages`.
    fn replace(&mut self, messages: Vec<Message>);

    /// Append a slice of messages (default: loops over `append`).
    fn append_many(&mut self, messages: Vec<Message>) {
        for msg in messages {
            self.append(msg);
        }
    }
}

// ── InMemoryConversation ──────────────────────────────────────────────────────

/// A simple `Vec`-backed [`Memory`] implementation.
///
/// Suitable for single-session use.  Not persisted across process restarts.
#[derive(Clone, Debug, Default)]
pub struct InMemoryConversation {
    history: Vec<Message>,
}

impl InMemoryConversation {
    /// Create an empty conversation.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a conversation seeded with an existing history.
    pub fn with_history(history: Vec<Message>) -> Self {
        Self { history }
    }

    /// How many messages are stored.
    pub fn len(&self) -> usize {
        self.history.len()
    }

    /// `true` when the conversation has no messages.
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }
}

impl Memory for InMemoryConversation {
    fn load(&self) -> Vec<Message> {
        self.history.clone()
    }

    fn append(&mut self, message: Message) {
        self.history.push(message);
    }

    fn replace(&mut self, messages: Vec<Message>) {
        self.history = messages;
    }
}

// ── Helper constructors ───────────────────────────────────────────────────────

/// Convenience: build a user text message.
pub fn user_message(text: impl Into<String>) -> Message {
    Message::user(text)
}

/// Convenience: build an assistant text message.
pub fn assistant_message(text: impl Into<String>) -> Message {
    Message::assistant(text)
}

/// Convenience: build a system message.
pub fn system_message(text: impl Into<String>) -> Message {
    Message::system(text)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_load() {
        let mut mem = InMemoryConversation::new();
        mem.append(user_message("hello"));
        mem.append(assistant_message("hi"));
        let history = mem.load();
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn replace_clears_history() {
        let mut mem = InMemoryConversation::new();
        mem.append(user_message("old"));
        mem.replace(vec![user_message("new")]);
        let history = mem.load();
        assert_eq!(history.len(), 1);
        assert!(matches!(&history[0], Message::User { .. }));
    }

    #[test]
    fn append_many() {
        let mut mem = InMemoryConversation::new();
        mem.append_many(vec![user_message("a"), assistant_message("b"), user_message("c")]);
        assert_eq!(mem.len(), 3);
    }

    #[test]
    fn is_empty() {
        let mem = InMemoryConversation::new();
        assert!(mem.is_empty());
    }

    #[test]
    fn with_history() {
        let mem = InMemoryConversation::with_history(vec![user_message("seed")]);
        assert_eq!(mem.len(), 1);
    }

    // Confirm Memory is object-safe.
    #[test]
    fn memory_is_object_safe() {
        let mem: Box<dyn Memory> = Box::new(InMemoryConversation::new());
        assert_eq!(mem.load().len(), 0);
    }
}
