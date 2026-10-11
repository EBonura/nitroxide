/// Shared settings rows for the front end and paused match.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SettingsRow {
    Arena,
    Sound,
    Music,
    Track,
    Brightness,
    ScreenX,
    ScreenY,
    Save,
    Back,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    Close,
    Save,
    Adjust(i8),
    None,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FormatAction {
    Confirm,
    Cancel,
    None,
}

/// Formatting has a separate consent edge. A simultaneous cancel always
/// leaves the card untouched, including when Cross is also newly pressed.
pub fn format_action(cross: bool, back: bool) -> FormatAction {
    if back { FormatAction::Cancel }
    else if cross { FormatAction::Confirm }
    else { FormatAction::None }
}

/// Only a new Cross press on Save authorizes a persistent write. Closing
/// takes precedence when Save and a cancel button arrive together.
pub fn action(row: SettingsRow, left: bool, right: bool, cross: bool, back: bool) -> Action {
    if back || (row == SettingsRow::Back && cross) {
        Action::Close
    } else if row == SettingsRow::Save {
        if cross { Action::Save } else { Action::None }
    } else if right || cross {
        Action::Adjust(1)
    } else if left {
        Action::Adjust(-1)
    } else {
        Action::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_and_navigation_never_authorize_a_card_write() {
        for row in [SettingsRow::Arena, SettingsRow::Sound, SettingsRow::Music,
            SettingsRow::Track, SettingsRow::Brightness, SettingsRow::ScreenX,
            SettingsRow::ScreenY, SettingsRow::Save, SettingsRow::Back] {
            assert_eq!(action(row, false, false, false, true), Action::Close);
            assert_eq!(action(row, false, false, true, true), Action::Close);
            assert_ne!(action(row, true, false, false, false), Action::Save);
            assert_ne!(action(row, false, true, false, false), Action::Save);
            assert_ne!(action(row, false, false, false, false), Action::Save);
        }
        assert_eq!(action(SettingsRow::Back, false, false, true, false), Action::Close);
    }

    #[test]
    fn only_explicit_save_selection_requests_persistence() {
        let events = [
            action(SettingsRow::Sound, false, false, true, false),
            action(SettingsRow::Back, false, false, true, false),
            action(SettingsRow::Save, false, false, false, false),
            action(SettingsRow::Save, false, true, false, false),
            action(SettingsRow::Save, false, false, true, false),
            action(SettingsRow::Save, false, false, true, true),
        ];
        assert_eq!(events.iter().filter(|event| **event == Action::Save).count(), 1);
        assert_eq!(events[0], Action::Adjust(1));
        assert_eq!(events[4], Action::Save);
    }

    #[test]
    fn canceled_format_consent_requires_a_distinct_new_confirmation() {
        let answers = [
            format_action(false, false),
            format_action(true, true),
            format_action(false, false),
            format_action(true, false),
        ];
        assert_eq!(answers, [FormatAction::None, FormatAction::Cancel,
            FormatAction::None, FormatAction::Confirm]);
        assert_eq!(answers.iter().filter(|answer| **answer == FormatAction::Confirm).count(), 1);
    }
}
