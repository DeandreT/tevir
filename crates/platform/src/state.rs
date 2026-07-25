use std::collections::HashSet;

use domain::{ButtonAction, InputEvent, InputKind, KeyAction, MouseButton, PhysicalKey};

#[derive(Debug, Default)]
pub(crate) struct HeldInput {
    keys: HashSet<PhysicalKey>,
    buttons: HashSet<MouseButton>,
}

impl HeldInput {
    pub(crate) fn observe(&mut self, event: InputEvent) {
        match event.kind {
            InputKind::Key { key, action } => match action {
                KeyAction::Press | KeyAction::Repeat { .. } => {
                    self.keys.insert(key);
                }
                KeyAction::Release => {
                    self.keys.remove(&key);
                }
            },
            InputKind::PointerButton { button, action } => match action {
                ButtonAction::Press => {
                    self.buttons.insert(button);
                }
                ButtonAction::Release => {
                    self.buttons.remove(&button);
                }
            },
            InputKind::PointerRelative { .. }
            | InputKind::PointerAbsolute(_)
            | InputKind::Scroll(_) => {}
        }
    }

    pub(crate) fn release_all(&mut self, elapsed_micros: u64) -> Vec<InputEvent> {
        let mut releases = Vec::with_capacity(self.keys.len() + self.buttons.len());
        releases.extend(self.keys.drain().map(|key| InputEvent {
            elapsed_micros,
            kind: InputKind::Key {
                key,
                action: KeyAction::Release,
            },
        }));
        releases.extend(self.buttons.drain().map(|button| InputEvent {
            elapsed_micros,
            kind: InputKind::PointerButton {
                button,
                action: ButtonAction::Release,
            },
        }));
        releases
    }
}

#[cfg(test)]
mod tests {
    use domain::{ButtonAction, InputEvent, InputKind, KeyAction, MouseButton, PhysicalKey};

    use super::HeldInput;

    #[test]
    fn releases_every_held_key_and_button() {
        let key = PhysicalKey::new(0x07, 0x04);
        let mut held = HeldInput::default();
        held.observe(InputEvent {
            elapsed_micros: 1,
            kind: InputKind::Key {
                key,
                action: KeyAction::Press,
            },
        });
        held.observe(InputEvent {
            elapsed_micros: 2,
            kind: InputKind::PointerButton {
                button: MouseButton::Primary,
                action: ButtonAction::Press,
            },
        });

        let releases = held.release_all(10);

        assert_eq!(releases.len(), 2);
        assert!(releases.iter().any(|event| {
            event.kind
                == InputKind::Key {
                    key,
                    action: KeyAction::Release,
                }
        }));
        assert!(releases.iter().any(|event| {
            event.kind
                == InputKind::PointerButton {
                    button: MouseButton::Primary,
                    action: ButtonAction::Release,
                }
        }));
        assert!(held.release_all(11).is_empty());
    }
}
