#[derive(Clone, Debug)]
pub struct Input {
    pub content: Vec<ash_core::Content>,
}

impl Input {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            content: vec![ash_core::Content::Text(text.into())],
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
            || self.content.iter().all(|content| match content {
                ash_core::Content::Text(text) => text.trim().is_empty(),
                ash_core::Content::Image { data, .. } => data.is_empty(),
            })
    }
}

impl From<String> for Input {
    fn from(value: String) -> Self {
        Self::user(value)
    }
}

impl From<&str> for Input {
    fn from(value: &str) -> Self {
        Self::user(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_only_text_is_empty() {
        assert!(Input::user(" \n\t").is_empty());
        assert!(!Input::user("  task  ").is_empty());
    }
}
