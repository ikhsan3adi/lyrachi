use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyScope {
    Global,
    Help,
    ContextMenu,
    SearchInput,
}

impl KeyScope {
    pub const INTERCEPT_ORDER: [Self; 4] = [
        Self::SearchInput,
        Self::ContextMenu,
        Self::Help,
        Self::Global,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyAction {
    Quit,
    Refetch,
    ToggleFullscreen,
    ToggleHeader,
    ToggleStatus,
    OffsetBack,
    OffsetForward,
    OffsetReset,
    ScrollUp,
    ScrollDown,
    ToggleHelp,
    CloseModal,
}

impl KeyAction {
    pub const ALL: &'static [Self] = &[
        Self::Quit,
        Self::Refetch,
        Self::ToggleFullscreen,
        Self::ToggleHeader,
        Self::ToggleStatus,
        Self::OffsetBack,
        Self::OffsetForward,
        Self::OffsetReset,
        Self::ScrollUp,
        Self::ScrollDown,
        Self::ToggleHelp,
        Self::CloseModal,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyTrigger {
    Char(char),
    ShiftChar(char),
    CtrlChar(char),
    Up,
    Down,
    Esc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyBinding {
    pub scope: KeyScope,
    pub trigger: KeyTrigger,
    pub action: KeyAction,
    pub key_label: &'static str,
    pub description: &'static str,
}

pub const KEYBINDINGS: &[KeyBinding] = &[
    KeyBinding {
        scope: KeyScope::Help,
        trigger: KeyTrigger::Esc,
        action: KeyAction::CloseModal,
        key_label: "Esc",
        description: "Close help overlay",
    },
    KeyBinding {
        scope: KeyScope::Help,
        trigger: KeyTrigger::Char('?'),
        action: KeyAction::CloseModal,
        key_label: "?",
        description: "Close help overlay",
    },
    KeyBinding {
        scope: KeyScope::Help,
        trigger: KeyTrigger::CtrlChar('c'),
        action: KeyAction::Quit,
        key_label: "Ctrl+C",
        description: "Quit",
    },
    KeyBinding {
        scope: KeyScope::Global,
        trigger: KeyTrigger::Char('?'),
        action: KeyAction::ToggleHelp,
        key_label: "?",
        description: "Toggle help overlay",
    },
    KeyBinding {
        scope: KeyScope::Global,
        trigger: KeyTrigger::Char('r'),
        action: KeyAction::Refetch,
        key_label: "r",
        description: "Refetch lyrics",
    },
    KeyBinding {
        scope: KeyScope::Global,
        trigger: KeyTrigger::Char('f'),
        action: KeyAction::ToggleFullscreen,
        key_label: "f",
        description: "Toggle fullscreen",
    },
    KeyBinding {
        scope: KeyScope::Global,
        trigger: KeyTrigger::Char('t'),
        action: KeyAction::ToggleHeader,
        key_label: "t",
        description: "Toggle title bar",
    },
    KeyBinding {
        scope: KeyScope::Global,
        trigger: KeyTrigger::Char('s'),
        action: KeyAction::ToggleStatus,
        key_label: "s",
        description: "Toggle status bar",
    },
    KeyBinding {
        scope: KeyScope::Global,
        trigger: KeyTrigger::Char('['),
        action: KeyAction::OffsetBack,
        key_label: "[",
        description: "Shift timing -100 ms",
    },
    KeyBinding {
        scope: KeyScope::Global,
        trigger: KeyTrigger::Char(']'),
        action: KeyAction::OffsetForward,
        key_label: "]",
        description: "Shift timing +100 ms",
    },
    KeyBinding {
        scope: KeyScope::Global,
        trigger: KeyTrigger::Char('\\'),
        action: KeyAction::OffsetReset,
        key_label: "\\",
        description: "Reset timing offset",
    },
    KeyBinding {
        scope: KeyScope::Global,
        trigger: KeyTrigger::Up,
        action: KeyAction::ScrollUp,
        key_label: "Up",
        description: "Scroll up (unsynced)",
    },
    KeyBinding {
        scope: KeyScope::Global,
        trigger: KeyTrigger::Down,
        action: KeyAction::ScrollDown,
        key_label: "Down",
        description: "Scroll down (unsynced)",
    },
    KeyBinding {
        scope: KeyScope::Global,
        trigger: KeyTrigger::Char('q'),
        action: KeyAction::Quit,
        key_label: "q",
        description: "Quit",
    },
    KeyBinding {
        scope: KeyScope::Global,
        trigger: KeyTrigger::CtrlChar('c'),
        action: KeyAction::Quit,
        key_label: "Ctrl+C",
        description: "Quit",
    },
];

pub fn normalize_key_event(key: KeyEvent) -> Option<KeyTrigger> {
    if key.kind != KeyEventKind::Press {
        return None;
    }

    let modifiers = key.modifiers;
    let has_ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let has_alt = modifiers.contains(KeyModifiers::ALT);
    let has_shift = modifiers.contains(KeyModifiers::SHIFT);

    if has_alt {
        return None;
    }

    match key.code {
        KeyCode::Up if modifiers.is_empty() => Some(KeyTrigger::Up),
        KeyCode::Down if modifiers.is_empty() => Some(KeyTrigger::Down),
        KeyCode::Esc if modifiers.is_empty() => Some(KeyTrigger::Esc),
        KeyCode::Char(ch) if has_ctrl => Some(KeyTrigger::CtrlChar(ch.to_ascii_lowercase())),
        KeyCode::Char('?') if !has_ctrl => Some(KeyTrigger::Char('?')),
        KeyCode::Char(ch) if ch.is_ascii_uppercase() => {
            Some(KeyTrigger::ShiftChar(ch.to_ascii_lowercase()))
        }
        KeyCode::Char(ch) if has_shift && ch.is_ascii_alphabetic() => {
            Some(KeyTrigger::ShiftChar(ch.to_ascii_lowercase()))
        }
        KeyCode::Char(ch) if !has_shift => Some(KeyTrigger::Char(ch)),
        _ => None,
    }
}

pub fn resolve_key(active_scope: KeyScope, key: KeyEvent) -> Option<KeyAction> {
    let trigger = normalize_key_event(key)?;
    KEYBINDINGS
        .iter()
        .find(|binding| binding.scope == active_scope && binding.trigger == trigger)
        .map(|binding| binding.action)
}

pub fn global_help_rows() -> Vec<(String, &'static str)> {
    let mut rows: Vec<(KeyAction, Vec<&'static str>, &'static str)> = Vec::new();
    for binding in KEYBINDINGS
        .iter()
        .filter(|binding| binding.scope == KeyScope::Global)
    {
        if let Some((_, labels, _)) = rows
            .iter_mut()
            .find(|(action, _, _)| *action == binding.action)
        {
            labels.push(binding.key_label);
        } else {
            rows.push((binding.action, vec![binding.key_label], binding.description));
        }
    }

    rows.into_iter()
        .map(|(_, labels, description)| (labels.join(", "), description))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn keybindings_have_no_duplicate_triggers_per_scope() {
        let mut seen = HashSet::new();
        for binding in KEYBINDINGS {
            assert!(
                seen.insert((binding.scope, binding.trigger)),
                "duplicate keybinding trigger {:?} in scope {:?}",
                binding.trigger,
                binding.scope
            );
        }
    }

    #[test]
    fn keybindings_cover_every_key_action() {
        for action in KeyAction::ALL {
            assert!(
                KEYBINDINGS.iter().any(|binding| binding.action == *action),
                "KeyAction::{action:?} is missing from KEYBINDINGS"
            );
        }
    }

    #[test]
    fn normalizes_shift_s_and_question_mark_consistently() {
        let shift_upper =
            normalize_key_event(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE));
        let shift_mod = normalize_key_event(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::SHIFT));
        assert_eq!(shift_upper, Some(KeyTrigger::ShiftChar('s')));
        assert_eq!(shift_upper, shift_mod);

        let q_plain = normalize_key_event(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        let q_shift = normalize_key_event(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT));
        assert_eq!(q_plain, Some(KeyTrigger::Char('?')));
        assert_eq!(q_plain, q_shift);

        assert_eq!(
            resolve_key(
                KeyScope::Global,
                KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT)
            ),
            Some(KeyAction::ToggleHelp)
        );
    }

    #[test]
    fn help_scope_blocks_global_hotkeys() {
        assert!(KeyScope::INTERCEPT_ORDER.starts_with(&[
            KeyScope::SearchInput,
            KeyScope::ContextMenu,
            KeyScope::Help,
            KeyScope::Global,
        ]));

        assert_eq!(
            resolve_key(
                KeyScope::Help,
                KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)
            ),
            Some(KeyAction::CloseModal)
        );
        assert_eq!(
            resolve_key(
                KeyScope::Help,
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
            ),
            Some(KeyAction::CloseModal)
        );
        assert_eq!(
            resolve_key(
                KeyScope::Help,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
            ),
            None
        );
        assert_eq!(
            resolve_key(
                KeyScope::Help,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Some(KeyAction::Quit)
        );
    }
}
