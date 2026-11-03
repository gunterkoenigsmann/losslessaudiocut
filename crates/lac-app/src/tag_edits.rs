//! What the user changed in a track's tags.
//!
//! A track's tags are the source's tags with the user's changes applied:
//! the changes are kept apart from the source's values, so that fields
//! the user never touched (and everything [`Tags`] keeps as extras) come
//! from the source, while a field the user cleared stays cleared. An
//! export opens the source itself, so it is only handed the changes and
//! applies them there ([`TagEdits::apply_to`]).

use std::collections::BTreeMap;

use lac_core::{TagField, TagValueError, Tags};

/// Changed fields of one track's tags: a field that is absent keeps the
/// source's value, an empty value clears it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagEdits(BTreeMap<TagField, String>);

impl TagEdits {
    /// No changes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether nothing is changed.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The changed value of `field`, `None` if it is unchanged. An empty
    /// string means "cleared".
    pub fn get(&self, field: TagField) -> Option<&str> {
        self.0.get(&field).map(String::as_str)
    }

    /// Sets `field` to `value` (trimmed; empty clears the field). Numeric
    /// fields must be a whole number or empty; nothing changes otherwise.
    pub fn set(&mut self, field: TagField, value: &str) -> Result<(), TagValueError> {
        // Validates the value the way `apply_to` will use it.
        Tags::default().set(field, value)?;
        self.0.insert(field, value.trim().to_string());
        Ok(())
    }

    /// Forgets the change of `field`: it keeps the source's value again.
    pub fn reset(&mut self, field: TagField) {
        self.0.remove(&field);
    }

    /// The changed fields and their values, in [`TagField`] order.
    pub fn iter(&self) -> impl Iterator<Item = (TagField, &str)> {
        self.0.iter().map(|(&f, v)| (f, v.as_str()))
    }

    /// `base` (usually the source's tags) with the changes applied.
    pub fn apply_to(&self, base: &Tags) -> Tags {
        let mut t = base.clone();
        for (f, v) in self.iter() {
            // `set` only stores values that `Tags::set` accepts.
            let _ = t.set(f, v);
        }
        t
    }
}

impl FromIterator<(TagField, String)> for TagEdits {
    /// Collects valid changes; invalid numeric values are skipped.
    fn from_iter<I: IntoIterator<Item = (TagField, String)>>(iter: I) -> Self {
        let mut e = TagEdits::new();
        for (f, v) in iter {
            let _ = e.set(f, &v);
        }
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> Tags {
        Tags {
            title: Some("Whole recording".into()),
            artist: Some("Speaker".into()),
            comment: Some("old".into()),
            extra: vec![("MOOD".into(), "calm".into())],
            ..Tags::default()
        }
    }

    #[test]
    fn untouched_fields_come_from_the_source() {
        let mut e = TagEdits::new();
        e.set(TagField::Title, " Part 1 ").expect("valid");
        e.set(TagField::TrackNumber, "1").expect("valid");
        e.set(TagField::Comment, "").expect("clearing is valid");
        let t = e.apply_to(&source());
        assert_eq!(t.title.as_deref(), Some("Part 1"));
        assert_eq!(t.artist.as_deref(), Some("Speaker"));
        assert_eq!(t.track_number, Some(1));
        assert_eq!(t.comment, None);
        assert_eq!(t.extra, source().extra);
        assert_eq!(e.get(TagField::Title), Some("Part 1"));
        assert_eq!(e.get(TagField::Comment), Some(""));
        assert_eq!(e.get(TagField::Artist), None);
    }

    #[test]
    fn invalid_numbers_are_rejected_and_reset_restores_the_source() {
        let mut e = TagEdits::new();
        assert!(e.set(TagField::TrackTotal, "ten").is_err());
        assert!(e.is_empty());
        e.set(TagField::Artist, "Other").expect("valid");
        e.reset(TagField::Artist);
        assert!(e.is_empty());
        assert_eq!(e.apply_to(&source()), source());
    }

    #[test]
    fn collects_from_pairs() {
        let e: TagEdits = [
            (TagField::Album, "A".to_string()),
            (TagField::TrackNumber, "x".to_string()),
        ]
        .into_iter()
        .collect();
        assert_eq!(e.iter().collect::<Vec<_>>(), vec![(TagField::Album, "A")]);
    }
}
