// SPDX-License-Identifier: GPL-3.0-or-later
//! The phone-input queue between a mirror window and the session outbox.
//! A wheel step that arrives while an earlier one at the same point still
//! waits joins it; nothing passes a touch, key or text, and the queue never
//! holds more than a fixed number of entries.

use std::{collections::VecDeque, sync::Mutex};

use nectarlink_core::{MirrorInput, TouchAction};
use tokio::sync::Notify;

use super::Window;

/// Entries waiting, at most.
pub(super) const MAX_QUEUED: usize = 1024;
/// The last entries only take a finger lifting, so a release still goes in
/// when everything else is refused.
pub(super) const RELEASE_ROOM: usize = 64;
/// Separate wheel steps waiting, at most: past this the phone is behind and
/// a step at yet another point would only arrive late.
pub(super) const MAX_SCROLLS: usize = 16;
/// The bridge's per-step bound; a sum past it waits as a step of its own.
const MAX_NOTCHES: f32 = 100.0;

/// An input with the owner it was admitted under (see `mirror::input`).
pub(super) struct Queued<C> {
    pub(super) window: Window,
    pub(super) control: C,
    pub(super) request: u64,
    pub(super) stream: u64,
    pub(super) input: MirrorInput,
}

pub(super) struct Inputs<C> {
    queue: Mutex<VecDeque<Queued<C>>>,
    ready: Notify,
}

impl<C> Inputs<C> {
    pub(super) fn new() -> Self {
        Self { queue: Mutex::new(VecDeque::new()), ready: Notify::new() }
    }

    /// Puts `entry` behind everything waiting, or adds its wheel step to the
    /// last entry. False when there is no room for it.
    pub(super) fn push(&self, entry: Queued<C>, current: impl Fn(&C) -> bool) -> bool {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(last) = queue.back_mut()
            && merge(last, &entry, &current)
        {
            return true;
        }
        let full = match &entry.input {
            MirrorInput::Touch { action: TouchAction::Up, .. } => queue.len() >= MAX_QUEUED,
            MirrorInput::Scroll { .. } => {
                queue.len() >= MAX_QUEUED - RELEASE_ROOM
                    || queue.iter().filter(|queued| matches!(queued.input, MirrorInput::Scroll { .. })).count()
                        >= MAX_SCROLLS
            }
            _ => queue.len() >= MAX_QUEUED - RELEASE_ROOM,
        };
        if full {
            return false;
        }
        queue.push_back(entry);
        drop(queue);
        self.ready.notify_one();
        true
    }

    pub(super) fn pop(&self) -> Option<Queued<C>> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front()
    }

    /// The oldest entry, once there is one; for the one task that sends them.
    pub(super) async fn next(&self) -> Queued<C> {
        loop {
            if let Some(queued) = self.pop() {
                return queued;
            }
            self.ready.notified().await;
        }
    }
}

/// Adds `next`'s step to `into`, which nothing has taken yet: both are wheel
/// steps at the exact same point of the same window, request and stream,
/// both owners are still current, and the sums stay in bounds.
fn merge<C>(into: &mut Queued<C>, next: &Queued<C>, current: impl Fn(&C) -> bool) -> bool {
    if into.window != next.window || into.request != next.request || into.stream != next.stream {
        return false;
    }
    let (
        MirrorInput::Scroll { x, y, dx, dy },
        &MirrorInput::Scroll { x: next_x, y: next_y, dx: next_dx, dy: next_dy },
    ) = (&mut into.input, &next.input)
    else {
        return false;
    };
    let (sum_dx, sum_dy) = (*dx + next_dx, *dy + next_dy);
    if x.to_bits() != next_x.to_bits()
        || y.to_bits() != next_y.to_bits()
        || !(sum_dx.abs() <= MAX_NOTCHES && sum_dy.abs() <= MAX_NOTCHES)
        || !current(&into.control)
        || !current(&next.control)
    {
        return false;
    }
    *dx = sum_dx;
    *dy = sum_dy;
    true
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use nectarlink_core::{DeviceId, MirrorInput, TouchAction};

    use super::{Inputs, MAX_QUEUED, MAX_SCROLLS, Queued, RELEASE_ROOM, Window};

    /// Stands in for `MirrorControl`, which the app can't mint without a
    /// phone owner's grant; clearing it is a revoke, Stop or epoch move.
    type Owner = Rc<Cell<bool>>;

    const REQUEST: u64 = 0x9E37_79B9_7F4A_7C15;
    const STREAM: u64 = 0x5EED;

    fn current(owner: &Owner) -> bool {
        owner.get()
    }

    fn entry(window: Window, owner: &Owner, request: u64, input: MirrorInput) -> Queued<Owner> {
        Queued { window, control: owner.clone(), request, stream: STREAM, input }
    }

    fn scroll(x: f32, dy: f32) -> MirrorInput {
        MirrorInput::Scroll { x, y: 0.5, dx: 0.0, dy }
    }

    fn up() -> MirrorInput {
        MirrorInput::Touch { action: TouchAction::Up, x: 0.5, y: 0.5 }
    }

    fn key() -> MirrorInput {
        MirrorInput::Key { key: "back".into() }
    }

    fn drained(inputs: &Inputs<Owner>) -> Vec<MirrorInput> {
        std::iter::from_fn(|| inputs.pop().map(|queued| queued.input)).collect()
    }

    #[test]
    fn wheel_steps_join_only_at_one_point_and_never_pass_a_release_or_key() {
        let window = Window::screen(DeviceId([0x5C; 32]));
        let owner: Owner = Rc::new(Cell::new(true));
        let inputs = Inputs::new();
        let push = |input| inputs.push(entry(window, &owner, REQUEST, input), current);
        for input in [
            scroll(0.4, 1.0),
            scroll(0.4, 2.0),
            scroll(0.6, 1.0),
            scroll(0.4, 4.0),
            up(),
            scroll(0.4, 8.0),
            key(),
            scroll(0.4, 16.0),
        ] {
            assert!(push(input));
        }
        assert_eq!(
            drained(&inputs),
            [scroll(0.4, 3.0), scroll(0.6, 1.0), scroll(0.4, 4.0), up(), scroll(0.4, 8.0), key(), scroll(0.4, 16.0)],
            "steps at one point add up; another point, a release or a key holds the line"
        );

        assert!(push(scroll(0.4, 1.0)));
        let sending = inputs.pop().map(|queued| queued.input);
        assert!(push(scroll(0.4, 1.0)), "a step after the one being sent waits on its own");
        assert_eq!(sending, Some(scroll(0.4, 1.0)));
        assert_eq!(drained(&inputs), [scroll(0.4, 1.0)]);

        for step in 0..MAX_SCROLLS {
            assert!(push(scroll(if step % 2 == 0 { 0.4 } else { 0.6 }, 1.0)));
        }
        assert!(!push(scroll(0.4, 1.0)), "a step at another point finds no room");
        assert!(push(scroll(0.6, 1.0)), "the last step at its own point still grows");
        assert!(push(up()), "wheel steps never take a release's room");
        assert!(push(key()), "or a key's");
        assert!(!push(scroll(0.6, 1.0)), "and nothing joins past them");
        let waiting = drained(&inputs);
        assert_eq!(waiting.len(), MAX_SCROLLS + 2);
        assert_eq!(waiting[MAX_SCROLLS - 1..], [scroll(0.6, 2.0), up(), key()]);
    }

    #[test]
    fn a_revoked_or_replaced_owner_never_absorbs_a_step_and_releases_keep_room() {
        let window = Window::screen(DeviceId([0x5D; 32]));
        let (old, new): (Owner, Owner) = (Rc::new(Cell::new(true)), Rc::new(Cell::new(true)));
        let inputs = Inputs::new();
        assert!(inputs.push(entry(window, &old, REQUEST, scroll(0.5, 2.0)), current));
        old.set(false);
        for next in [
            entry(window, &new, REQUEST, scroll(0.5, 1.0)),
            entry(window, &new, REQUEST + 1, scroll(0.5, 1.0)),
            Queued { stream: STREAM + 1, ..entry(window, &new, REQUEST + 1, scroll(0.5, 1.0)) },
            entry(Window { session: 7, ..window }, &new, REQUEST + 1, scroll(0.5, 1.0)),
            entry(Window { session: 7, ..window }, &new, REQUEST + 1, scroll(0.5, 100.0)),
        ] {
            assert!(inputs.push(next, current));
        }
        let waiting: Vec<_> = std::iter::from_fn(|| inputs.pop()).collect();
        assert_eq!(waiting.len(), 6, "a regrant, another request, stream or window, or a sum past the bound waits alone");
        assert!(!waiting[0].control.get() && waiting[0].input == scroll(0.5, 2.0), "the revoked step stays alone, for the sender to discard");
        assert!(waiting[1..].iter().all(|queued| queued.control.get()));

        let inputs = Inputs::new();
        let push = |input| inputs.push(entry(window, &new, REQUEST, input), current);
        for _ in 0..MAX_QUEUED - RELEASE_ROOM {
            assert!(push(key()));
        }
        assert!(!push(key()), "past the shared room a key is refused");
        assert!(!push(scroll(0.5, 1.0)));
        for _ in 0..RELEASE_ROOM {
            assert!(push(up()), "a release still goes in");
        }
        assert!(!push(up()), "the queue never holds more than MAX_QUEUED");
        assert_eq!(drained(&inputs).len(), MAX_QUEUED);
    }
}
