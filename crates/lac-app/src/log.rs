//! The log pane's view model: the messages the status bar showed, each
//! with the time of day, kept to a bounded number of lines.
//!
//! The status bar shows one message at a time and the next one replaces
//! it; the log keeps them, so a warning that flashed by while the user
//! looked elsewhere can still be read.

use std::collections::VecDeque;

/// What the log's text control has to do after [`Log::push`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// Nothing (an empty message, or the same as the last one).
    None,
    /// Append this line (it ends in a newline).
    Append(String),
    /// The log was full and dropped its oldest lines: replace the whole
    /// text with this.
    Replace(String),
}

/// The messages, oldest first.
#[derive(Debug, Clone)]
pub struct Log {
    lines: VecDeque<String>,
    /// The text of the last message, without its time, to drop repeats.
    last: Option<String>,
    /// At most this many lines are kept.
    max: usize,
}

impl Log {
    /// An empty log keeping at most `max` lines (at least 2).
    pub fn new(max: usize) -> Self {
        Log {
            lines: VecDeque::new(),
            last: None,
            max: max.max(2),
        }
    }

    /// Adds `text`, shown at `hour:minute:second`. A message equal to
    /// the previous one is left out (a timer may report the same state
    /// again). When the log is full, the oldest half goes, so the text
    /// control is rewritten only now and then.
    pub fn push(&mut self, (h, m, s): (i16, i16, i16), text: &str) -> Update {
        let text = text.trim();
        if text.is_empty() || self.last.as_deref() == Some(text) {
            return Update::None;
        }
        self.last = Some(text.to_string());
        let line = format!("{h:02}:{m:02}:{s:02}  {text}\n");
        self.lines.push_back(line.clone());
        if self.lines.len() <= self.max {
            return Update::Append(line);
        }
        let keep = self.max / 2;
        let drop = self.lines.len() - keep;
        self.lines.drain(..drop);
        Update::Replace(self.text())
    }

    /// The whole log as text.
    pub fn text(&self) -> String {
        self.lines.iter().map(String::as_str).collect()
    }

    /// Number of lines kept.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether nothing was logged (or everything was dropped).
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: (i16, i16, i16) = (9, 5, 7);

    #[test]
    fn lines_carry_the_time() {
        let mut log = Log::new(10);
        assert_eq!(
            log.push(T, "talk.mp3 (waveform from cache)"),
            Update::Append("09:05:07  talk.mp3 (waveform from cache)\n".to_string())
        );
        assert_eq!(log.text(), "09:05:07  talk.mp3 (waveform from cache)\n");
    }

    #[test]
    fn repeats_and_empty_messages_are_left_out() {
        let mut log = Log::new(10);
        assert!(matches!(log.push(T, "nothing to undo"), Update::Append(_)));
        assert_eq!(log.push((9, 5, 8), "nothing to undo"), Update::None);
        assert_eq!(log.push(T, "  "), Update::None);
        assert!(matches!(log.push(T, "exported"), Update::Append(_)));
        assert!(matches!(log.push(T, "nothing to undo"), Update::Append(_)));
        assert_eq!(log.len(), 3);
    }

    #[test]
    fn a_full_log_drops_its_older_half() {
        let mut log = Log::new(4);
        for i in 0..4 {
            assert!(matches!(log.push(T, &i.to_string()), Update::Append(_)));
        }
        let Update::Replace(text) = log.push(T, "4") else {
            panic!("expected a rewrite");
        };
        assert_eq!(text, "09:05:07  3\n09:05:07  4\n");
        assert_eq!(log.len(), 2);
        assert!(matches!(log.push(T, "5"), Update::Append(_)));
    }
}
