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
    ToggleDisplayMode,
    ToggleWordHighlight,
    ToggleRibbon,
    OffsetBack,
    OffsetForward,
    OffsetReset,
    ScrollUp,
    ScrollDown,
    ToggleHelp,
    CloseModal,
    OpenContextMenu,
    MenuUp,
    MenuDown,
    MenuSelect,
    SearchSwitchField,
    SearchBackspace,
    SearchSubmit,
}

impl KeyAction {
    pub const ALL: &'static [Self] = &[
        Self::Quit,
        Self::Refetch,
        Self::ToggleFullscreen,
        Self::ToggleHeader,
        Self::ToggleStatus,
        Self::ToggleDisplayMode,
        Self::ToggleWordHighlight,
        Self::ToggleRibbon,
        Self::OffsetBack,
        Self::OffsetForward,
        Self::OffsetReset,
        Self::ScrollUp,
        Self::ScrollDown,
        Self::ToggleHelp,
        Self::CloseModal,
        Self::OpenContextMenu,
        Self::MenuUp,
        Self::MenuDown,
        Self::MenuSelect,
        Self::SearchSwitchField,
        Self::SearchBackspace,
        Self::SearchSubmit,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyTrigger {
    Char(char),
    ShiftChar(char),
    CtrlChar(char),
    Up,
    Down,
    Enter,
    Backspace,
    Esc,
    Tab,
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
        scope: KeyScope::ContextMenu,
        trigger: KeyTrigger::Up,
        action: KeyAction::MenuUp,
        key_label: "Up",
        description: "Move selection up",
    },
    KeyBinding {
        scope: KeyScope::ContextMenu,
        trigger: KeyTrigger::Down,
        action: KeyAction::MenuDown,
        key_label: "Down",
        description: "Move selection down",
    },
    KeyBinding {
        scope: KeyScope::ContextMenu,
        trigger: KeyTrigger::Enter,
        action: KeyAction::MenuSelect,
        key_label: "Enter",
        description: "Select menu item",
    },
    KeyBinding {
        scope: KeyScope::ContextMenu,
        trigger: KeyTrigger::Esc,
        action: KeyAction::CloseModal,
        key_label: "Esc",
        description: "Close context menu",
    },
    KeyBinding {
        scope: KeyScope::ContextMenu,
        trigger: KeyTrigger::CtrlChar('c'),
        action: KeyAction::Quit,
        key_label: "Ctrl+C",
        description: "Quit",
    },
    KeyBinding {
        scope: KeyScope::SearchInput,
        trigger: KeyTrigger::Tab,
        action: KeyAction::SearchSwitchField,
        key_label: "Tab",
        description: "Switch search field",
    },
    KeyBinding {
        scope: KeyScope::SearchInput,
        trigger: KeyTrigger::Up,
        action: KeyAction::SearchSwitchField,
        key_label: "Up",
        description: "Switch search field",
    },
    KeyBinding {
        scope: KeyScope::SearchInput,
        trigger: KeyTrigger::Down,
        action: KeyAction::SearchSwitchField,
        key_label: "Down",
        description: "Switch search field",
    },
    KeyBinding {
        scope: KeyScope::SearchInput,
        trigger: KeyTrigger::Backspace,
        action: KeyAction::SearchBackspace,
        key_label: "Backspace",
        description: "Delete character",
    },
    KeyBinding {
        scope: KeyScope::SearchInput,
        trigger: KeyTrigger::Enter,
        action: KeyAction::SearchSubmit,
        key_label: "Enter",
        description: "Submit search",
    },
    KeyBinding {
        scope: KeyScope::SearchInput,
        trigger: KeyTrigger::Esc,
        action: KeyAction::CloseModal,
        key_label: "Esc",
        description: "Cancel search",
    },
    KeyBinding {
        scope: KeyScope::SearchInput,
        trigger: KeyTrigger::CtrlChar('c'),
        action: KeyAction::Quit,
        key_label: "Ctrl+C",
        description: "Quit",
    },
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
        trigger: KeyTrigger::Char('m'),
        action: KeyAction::ToggleDisplayMode,
        key_label: "m",
        description: "Toggle display mode (normal/block)",
    },
    KeyBinding {
        scope: KeyScope::Global,
        trigger: KeyTrigger::Char('w'),
        action: KeyAction::ToggleWordHighlight,
        key_label: "w",
        description: "Toggle word highlight",
    },
    KeyBinding {
        scope: KeyScope::Global,
        trigger: KeyTrigger::Tab,
        action: KeyAction::ToggleRibbon,
        key_label: "Tab",
        description: "Toggle ribbon bar",
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
        trigger: KeyTrigger::Char('c'),
        action: KeyAction::OpenContextMenu,
        key_label: "c",
        description: "Open context menu",
    },
    KeyBinding {
        scope: KeyScope::Global,
        trigger: KeyTrigger::Enter,
        action: KeyAction::OpenContextMenu,
        key_label: "Enter",
        description: "Open context menu",
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
        KeyCode::Up if !has_ctrl && !has_alt && !has_shift => Some(KeyTrigger::Up),
        KeyCode::Down if !has_ctrl && !has_alt && !has_shift => Some(KeyTrigger::Down),
        KeyCode::Enter if !has_ctrl && !has_alt && !has_shift => Some(KeyTrigger::Enter),
        KeyCode::Backspace if !has_ctrl && !has_alt && !has_shift => Some(KeyTrigger::Backspace),
        KeyCode::Esc if !has_ctrl && !has_alt && !has_shift => Some(KeyTrigger::Esc),
        KeyCode::Tab if !has_ctrl && !has_alt && !has_shift => Some(KeyTrigger::Tab),
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

pub fn search_input_char(key: KeyEvent) -> Option<char> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let modifiers = key.modifiers;
    if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return None;
    }
    match key.code {
        KeyCode::Char(ch) if !ch.is_control() => Some(ch),
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

        let tab_key = normalize_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(tab_key, Some(KeyTrigger::Tab));
        let tab_ctrl = normalize_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL));
        assert_eq!(tab_ctrl, None);
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

    #[test]
    fn context_menu_and_search_input_block_global_hotkeys() {
        assert_eq!(
            resolve_key(
                KeyScope::Global,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)
            ),
            Some(KeyAction::OpenContextMenu)
        );
        assert_eq!(
            resolve_key(
                KeyScope::Global,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            Some(KeyAction::OpenContextMenu)
        );

        for forbidden in ['q', 'r', 's', 'm', 'w', '?', 'c', 'S', '[', ']', '\\'] {
            let mods = if forbidden.is_ascii_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            let ev = KeyEvent::new(KeyCode::Char(forbidden), mods);
            assert_eq!(resolve_key(KeyScope::ContextMenu, ev), None);
            assert_eq!(resolve_key(KeyScope::SearchInput, ev), None);
        }

        let input = "qrs mw?cS[]\\";
        let mut collected = String::new();
        for ch in input.chars() {
            let mods = if ch.is_ascii_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            let ev = KeyEvent::new(KeyCode::Char(ch), mods);
            assert_eq!(resolve_key(KeyScope::SearchInput, ev), None);
            if let Some(captured) = search_input_char(ev) {
                collected.push(captured);
            }
        }
        assert_eq!(collected, input);
        assert_eq!(
            search_input_char(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            resolve_key(
                KeyScope::SearchInput,
                KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)
            ),
            Some(KeyAction::SearchSwitchField)
        );
        assert_eq!(
            resolve_key(
                KeyScope::SearchInput,
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)
            ),
            Some(KeyAction::SearchSwitchField)
        );
        assert_eq!(
            resolve_key(
                KeyScope::SearchInput,
                KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)
            ),
            Some(KeyAction::SearchSwitchField)
        );
    }
}
