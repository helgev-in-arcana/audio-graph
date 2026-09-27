use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

/// The modules already loaded on one thread, by [`identity`][crate::identity].
///
/// A module's entry point initialises process-wide state and has to be
/// balanced exactly once however many times a host asks for the module:
/// loading it twice and dropping one handle would run the exit point while
/// the other is still in use. So a second open returns the module already
/// held. `Weak`, so a module is really unloaded once nothing refers to it.
///
/// Per thread because a module's objects are pinned to the thread that loaded
/// it; each backend keeps one in a `thread_local!` of its own type.
pub struct Loaded<T>(RefCell<HashMap<PathBuf, Weak<T>>>);

impl<T> Default for Loaded<T> {
    fn default() -> Self {
        Loaded(RefCell::new(HashMap::new()))
    }
}

impl<T> Loaded<T> {
    /// The module loaded under `key`, if something still holds it.
    pub fn get(&self, key: &Path) -> Option<Rc<T>> {
        self.0.borrow().get(key).and_then(Weak::upgrade)
    }

    /// Record a module just loaded.
    pub fn insert(&self, key: PathBuf, module: &Rc<T>) {
        self.0.borrow_mut().insert(key, Rc::downgrade(module));
    }
}
