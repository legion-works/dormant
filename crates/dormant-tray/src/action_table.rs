//! Tagged projection of the platform-neutral menu model.

use crate::icon::Glyph;
use crate::menu::{Action, MenuEntry};

/// A menu entry annotated with the target/action tag of an action leaf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaggedMenuEntry {
    /// A clickable action with its one-based target/action tag.
    Action {
        /// Visible menu label.
        label: String,
        /// Whether the action is currently available.
        enabled: bool,
        /// Icon associated with the action.
        icon: Glyph,
        /// Platform-neutral action resolved by the tag.
        action: Action,
        /// Positive, one-based action-table index.
        tag: isize,
    },
    /// A visual separator.
    Separator,
    /// A recursively nested menu.
    Submenu {
        /// Visible submenu label.
        label: String,
        /// Whether the submenu can be opened.
        enabled: bool,
        /// Nested menu entries.
        entries: Vec<TaggedMenuEntry>,
    },
    /// A disabled informational entry.
    Info {
        /// Visible informational label.
        label: String,
        /// Icon associated with the information.
        icon: Glyph,
    },
}

/// Resolves platform target/action tags back to menu actions.
#[derive(Debug, Default)]
pub struct ActionTable {
    actions: Vec<Action>,
}

impl ActionTable {
    /// Replace actions from a menu model and return its tagged projection.
    #[must_use]
    pub fn replace_from_menu(&mut self, entries: &[MenuEntry]) -> Vec<TaggedMenuEntry> {
        self.actions.clear();
        entries.iter().map(|entry| self.tag_entry(entry)).collect()
    }

    /// Resolve a positive target/action tag to its action.
    #[must_use]
    pub fn resolve(&self, tag: isize) -> Option<Action> {
        if tag <= 0 {
            return None;
        }

        let index = usize::try_from(tag - 1).ok()?;
        self.actions.get(index).cloned()
    }

    fn tag_entry(&mut self, entry: &MenuEntry) -> TaggedMenuEntry {
        match entry {
            MenuEntry::Action {
                label,
                enabled,
                icon,
                action,
            } => {
                self.actions.push(action.clone());
                let tag = isize::try_from(self.actions.len()).unwrap_or(isize::MAX);
                TaggedMenuEntry::Action {
                    label: label.clone(),
                    enabled: *enabled,
                    icon: *icon,
                    action: action.clone(),
                    tag,
                }
            }
            MenuEntry::Separator => TaggedMenuEntry::Separator,
            MenuEntry::Submenu {
                label,
                enabled,
                entries,
            } => TaggedMenuEntry::Submenu {
                label: label.clone(),
                enabled: *enabled,
                entries: entries.iter().map(|entry| self.tag_entry(entry)).collect(),
            },
            MenuEntry::Info { label, icon } => TaggedMenuEntry::Info {
                label: label.clone(),
                icon: *icon,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ActionTable, TaggedMenuEntry};
    use crate::icon::Glyph;
    use crate::menu::{Action, MenuEntry};

    #[test]
    fn tags_resolve_nested_actions_without_tagging_info_or_separators() {
        let model = vec![
            MenuEntry::Info {
                label: "status".into(),
                icon: Glyph::Info,
            },
            MenuEntry::Separator,
            MenuEntry::Submenu {
                label: "display-a".into(),
                enabled: true,
                entries: vec![
                    MenuEntry::Action {
                        label: "Blank now".into(),
                        enabled: true,
                        icon: Glyph::DisplayOff,
                        action: Action::BlankOne("display-a".into()),
                    },
                    MenuEntry::Action {
                        label: "Wake now".into(),
                        enabled: true,
                        icon: Glyph::DisplayOn,
                        action: Action::WakeOne("display-a".into()),
                    },
                ],
            },
        ];
        let mut table = ActionTable::default();
        let tagged = table.replace_from_menu(&model);
        assert!(matches!(&tagged[0], TaggedMenuEntry::Info { .. }));
        assert!(matches!(&tagged[1], TaggedMenuEntry::Separator));
        let TaggedMenuEntry::Submenu { entries, .. } = &tagged[2] else {
            panic!("expected display submenu")
        };
        assert!(matches!(
            &entries[0],
            TaggedMenuEntry::Action { tag: 1, .. }
        ));
        assert!(matches!(
            &entries[1],
            TaggedMenuEntry::Action { tag: 2, .. }
        ));
        assert_eq!(table.resolve(1), Some(Action::BlankOne("display-a".into())));
        assert_eq!(table.resolve(2), Some(Action::WakeOne("display-a".into())));
    }

    #[test]
    fn invalid_or_untagged_values_are_safe_no_ops() {
        let mut table = ActionTable::default();
        let _ = table.replace_from_menu(&[MenuEntry::Action {
            label: "Quit".into(),
            enabled: true,
            icon: Glyph::Exit,
            action: Action::Quit,
        }]);
        assert_eq!(table.resolve(-1), None);
        assert_eq!(table.resolve(0), None);
        assert_eq!(table.resolve(999_999), None);
    }
}
