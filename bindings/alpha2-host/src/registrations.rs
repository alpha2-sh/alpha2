//! Runtime effects and effect resources on the shared renderer, held by the requests that use them.
//!
//! A request holds each id under the identity it defines it with (an effect's definition, a
//! resource's bytes). Two requests holding one id share its registration when their identities
//! match; otherwise the later one waits until the earlier releases it. Registrations nobody holds
//! stay for reuse until a request that does not want them registers its own.

use std::collections::HashMap;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Key {
    Effect(String, u32),
    Resource(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    Effect,
    Resource,
}

impl Key {
    pub(crate) const fn kind(&self) -> Kind {
        match self {
            Self::Effect(..) => Kind::Effect,
            Self::Resource(_) => Kind::Resource,
        }
    }
}

struct Live {
    identity: String,
    users: usize,
    /// Why registering it failed; nothing is registered then.
    refused: Option<String>,
}

#[derive(Default)]
pub(crate) struct Registrations {
    live: Mutex<HashMap<Key, Live>>,
    released: Condvar,
}

impl Registrations {
    fn lock(&self) -> MutexGuard<'_, HashMap<Key, Live>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Holds every `wanted` key of `kind` under its identity, and returns, per key, why its
    /// registration was refused, if it was.
    ///
    /// Waits while another request holds a wanted key under another identity. `register` runs for
    /// a key not registered under its identity, or registered by nobody's hold with a refusal;
    /// `unregister` for each registration of `kind` nobody holds that `wanted` does not name as it
    /// is. Each key goes into `held` as soon as its count is taken, so a caller that releases
    /// `held` on unwind releases every count a panicking `register` left.
    pub(crate) fn hold(
        &self,
        kind: Kind,
        wanted: &[(Key, String)],
        held: &mut Vec<Key>,
        register: &mut dyn FnMut(&Key) -> Option<String>,
        unregister: &mut dyn FnMut(&Key),
    ) -> Vec<Option<String>> {
        debug_assert!(wanted.iter().all(|(key, _)| key.kind() == kind));
        let mut live = self.lock();
        while wanted.iter().any(|(key, identity)| {
            live.get(key)
                .is_some_and(|held| held.users > 0 && held.identity != *identity)
        }) {
            live = self
                .released
                .wait(live)
                .unwrap_or_else(PoisonError::into_inner);
        }
        let stale = live
            .iter()
            .filter(|(key, live)| {
                live.users == 0
                    && key.kind() == kind
                    && (live.refused.is_some()
                        || wanted
                            .iter()
                            .find(|(wanted, _)| wanted == *key)
                            .is_none_or(|(_, identity)| *identity != live.identity))
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in stale {
            if live.remove(&key).is_some_and(|held| held.refused.is_none()) {
                unregister(&key);
            }
        }
        wanted
            .iter()
            .map(|(key, identity)| {
                let refused = if let Some(shared) = live.get_mut(key) {
                    shared.users += 1;
                    shared.refused.clone()
                } else {
                    let refused = register(key);
                    live.insert(
                        key.clone(),
                        Live {
                            identity: identity.clone(),
                            users: 1,
                            refused: refused.clone(),
                        },
                    );
                    refused
                };
                held.push(key.clone());
                refused
            })
            .collect()
    }

    pub(crate) fn release(&self, keys: &[Key]) {
        if keys.is_empty() {
            return;
        }
        let mut live = self.lock();
        for key in keys {
            if let Some(held) = live.get_mut(key) {
                held.users = held.users.saturating_sub(1);
            }
        }
        drop(live);
        self.released.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn effect(id: &str) -> Key {
        Key::Effect(id.to_owned(), 1)
    }

    #[test]
    fn one_identity_is_shared_and_another_waits_for_its_release() {
        let registrations = Arc::new(Registrations::default());
        let mut registered = Vec::new();
        let first = registrations.hold(
            Kind::Effect,
            &[(effect("tint"), "red".to_owned())],
            &mut Vec::new(),
            &mut |key| {
                registered.push(key.clone());
                None
            },
            &mut |_| panic!("nothing to unregister"),
        );
        assert_eq!(first, [None]);
        let shared = registrations.hold(
            Kind::Effect,
            &[(effect("tint"), "red".to_owned())],
            &mut Vec::new(),
            &mut |_| panic!("registered once"),
            &mut |_| panic!("held"),
        );
        assert_eq!(shared, [None]);
        assert_eq!(registered, [effect("tint")]);
        let (sender, receiver) = std::sync::mpsc::channel();
        let waiting = {
            let registrations = Arc::clone(&registrations);
            std::thread::spawn(move || {
                registrations.hold(
                    Kind::Effect,
                    &[(effect("tint"), "blue".to_owned())],
                    &mut Vec::new(),
                    &mut |_| {
                        sender.send("registered").expect("send");
                        Some("refused".to_owned())
                    },
                    &mut |_| sender.send("unregistered").expect("send"),
                )
            })
        };
        assert!(receiver.recv_timeout(Duration::from_millis(200)).is_err());
        registrations.release(&[effect("tint")]);
        assert!(receiver.recv_timeout(Duration::from_millis(200)).is_err());
        registrations.release(&[effect("tint")]);
        assert_eq!(waiting.join().expect("join"), [Some("refused".to_owned())]);
        assert_eq!(
            receiver.try_iter().collect::<Vec<_>>(),
            ["unregistered", "registered"]
        );
    }

    #[test]
    fn idle_registrations_of_the_same_kind_go_when_a_request_wants_others() {
        let registrations = Registrations::default();
        registrations.hold(
            Kind::Effect,
            &[(effect("old"), "a".to_owned())],
            &mut Vec::new(),
            &mut |_| None,
            &mut |_| {},
        );
        registrations.hold(
            Kind::Resource,
            &[(Key::Resource("lut".to_owned()), "bytes".to_owned())],
            &mut Vec::new(),
            &mut |_| None,
            &mut |_| {},
        );
        registrations.release(&[effect("old"), Key::Resource("lut".to_owned())]);
        let mut unregistered = Vec::new();
        registrations.hold(
            Kind::Effect,
            &[],
            &mut Vec::new(),
            &mut |_| None,
            &mut |key| {
                unregistered.push(key.clone());
            },
        );
        assert_eq!(unregistered, [effect("old")]);
    }

    #[test]
    fn a_registration_that_panics_leaves_no_count_its_holder_cannot_release() {
        let registrations = Arc::new(Registrations::default());
        let mut held = Vec::new();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registrations.hold(
                Kind::Effect,
                &[
                    (effect("first"), "a".to_owned()),
                    (effect("second"), "a".to_owned()),
                ],
                &mut held,
                &mut |key| {
                    assert_ne!(*key, effect("second"), "injected registration panic");
                    None
                },
                &mut |_| {},
            )
        }));
        assert!(unwound.is_err());
        registrations.release(&held);
        let (sender, receiver) = std::sync::mpsc::channel();
        let other = Arc::clone(&registrations);
        std::thread::spawn(move || {
            let held = other.hold(
                Kind::Effect,
                &[(effect("first"), "b".to_owned())],
                &mut Vec::new(),
                &mut |_| None,
                &mut |_| {},
            );
            let _ = sender.send(held);
        });
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("not left waiting"),
            [None]
        );
    }

    #[test]
    fn a_refusal_nobody_holds_is_registered_again() {
        let registrations = Registrations::default();
        let mut held = Vec::new();
        let refused = registrations.hold(
            Kind::Resource,
            &[(Key::Resource("lut".to_owned()), "bytes".to_owned())],
            &mut held,
            &mut |_| Some("device busy".to_owned()),
            &mut |_| panic!("a refusal is not registered"),
        );
        assert_eq!(refused, [Some("device busy".to_owned())]);
        registrations.release(&held);
        let mut registered = 0;
        let again = registrations.hold(
            Kind::Resource,
            &[(Key::Resource("lut".to_owned()), "bytes".to_owned())],
            &mut Vec::new(),
            &mut |_| {
                registered += 1;
                None
            },
            &mut |_| panic!("a refusal is not registered"),
        );
        assert_eq!((again, registered), (vec![None], 1));
    }
}
