use domain::{InputEvent, InputKind};
use protocol::MAX_INPUT_EVENTS_PER_BATCH;

#[derive(Debug, Default)]
pub(crate) struct EventBuffer {
    events: Vec<InputEvent>,
}

impl EventBuffer {
    pub(crate) fn push(&mut self, event: InputEvent) -> bool {
        if let Some(previous) = self.events.last_mut() {
            match (&mut previous.kind, event.kind) {
                (
                    InputKind::PointerRelative {
                        dx_micropixels: previous_x,
                        dy_micropixels: previous_y,
                    },
                    InputKind::PointerRelative {
                        dx_micropixels,
                        dy_micropixels,
                    },
                ) => {
                    *previous_x = previous_x.saturating_add(dx_micropixels);
                    *previous_y = previous_y.saturating_add(dy_micropixels);
                    previous.elapsed_micros = previous.elapsed_micros.max(event.elapsed_micros);
                    return self.events.len() >= MAX_INPUT_EVENTS_PER_BATCH;
                }
                (InputKind::PointerAbsolute(previous_point), InputKind::PointerAbsolute(point)) => {
                    *previous_point = point;
                    previous.elapsed_micros = previous.elapsed_micros.max(event.elapsed_micros);
                    return self.events.len() >= MAX_INPUT_EVENTS_PER_BATCH;
                }
                _ => {}
            }
        }

        self.events.push(event);
        self.events.len() >= MAX_INPUT_EVENTS_PER_BATCH
    }

    pub(crate) fn take(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.events)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use domain::{InputEvent, InputKind, KeyAction, PhysicalKey};

    use super::EventBuffer;

    #[test]
    fn coalesces_only_adjacent_pointer_motion() {
        let mut buffer = EventBuffer::default();
        buffer.push(relative(1, 10, -5));
        buffer.push(relative(2, 20, 8));
        buffer.push(InputEvent {
            elapsed_micros: 3,
            kind: InputKind::Key {
                key: PhysicalKey::new(0x07, 0x04),
                action: KeyAction::Press,
            },
        });
        buffer.push(relative(4, 2, 3));

        let events = buffer.take();
        assert_eq!(events.len(), 3);
        assert_eq!(
            events[0],
            InputEvent {
                elapsed_micros: 2,
                kind: InputKind::PointerRelative {
                    dx_micropixels: 30,
                    dy_micropixels: 3,
                },
            }
        );
        assert!(matches!(events[1].kind, InputKind::Key { .. }));
        assert_eq!(events[2], relative(4, 2, 3));
    }

    fn relative(elapsed_micros: u64, dx_micropixels: i64, dy_micropixels: i64) -> InputEvent {
        InputEvent {
            elapsed_micros,
            kind: InputKind::PointerRelative {
                dx_micropixels,
                dy_micropixels,
            },
        }
    }
}
