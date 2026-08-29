//! The snapshot the editor draws from.
//!
//! Everything the editor shows about a sub-plugin comes from the sub-plugin
//! host, which only the main thread may touch — and the editor is not always on
//! it: baseview gives it a thread of its own on X11. So the main thread builds
//! this and the editor copies out of it.
//!
//! Its own module so that neither of the two sides has to depend on the other:
//! `shared` publishes it and `editor` reads it, and this file knows about
//! neither.

use audio_graph_engine::InstanceView;
use plugin_host::ParamInfo;
use subhost_adapter::SubHost;

/// What the editor draws, as the main thread last saw it.
///
/// Everything here comes from the sub-plugin host, which only the main thread
/// may touch — and the editor is not always on it: baseview gives it a thread
/// of its own on X11. So the main thread rebuilds this on its tick and the
/// editor copies out of it, which also keeps large parameter lists from being
/// rebuilt sixty times a second when nothing has changed.
#[derive(Default, Clone)]
pub(crate) struct View {
    /// The [`Shared::generation`] the vectors below were built from.
    pub(crate) generation: u64,
    pub(crate) class: Option<(String, String)>,
    pub(crate) loaded: bool,
    pub(crate) params: Vec<ParamInfo>,
    /// `(index, parameter name, currently resolved)`.
    pub(crate) slots: Vec<(usize, String, bool)>,
    /// One entry per instance slot, used by plugin nodes for rendering.
    pub(crate) instances: Vec<InstanceView>,
    pub(crate) free_instance: Option<usize>,
    /// Whether the sub-plugin supports per-voice modulation.
    pub(crate) poly_modulation: bool,
}
impl View {
    /// Read the sub-plugin host into a snapshot. Main thread only.
    ///
    /// Cheap fields every time; the vectors only when `generation` says
    /// something changed shape.
    ///
    /// Takes the host and nothing else, which is what keeps this module below
    /// both the state and the editor rather than between them.
    pub(crate) fn rebuild(&mut self, host: &SubHost, generation: u64) {
        self.class = host.class(0).map(|c| (c.name.clone(), c.vendor.clone()));
        self.loaded = host.is_loaded(0);
        self.poly_modulation = host.capabilities(0).poly_modulation;
        self.free_instance = host.free_instance();

        // Whether a sub-plugin's window is open changes with nobody asking:
        // the user can close it from its own title bar, and the only thing that
        // notices is the tick. It cannot sit behind `generation`, which only
        // moves when a command runs.
        for (instance, view) in self.instances.iter_mut().enumerate() {
            view.editor_open = host.editor_is_open(instance);
        }

        if generation == self.generation && !self.params.is_empty() {
            return;
        }
        self.generation = generation;
        self.params = host.params(0).to_vec();
        self.instances = (0..crate::config::MAX_INSTANCES)
            .map(|i| InstanceView {
                loaded: host.is_loaded(i),
                name: host.class(i).map_or_else(String::new, |c| c.name.clone()),
                editor_open: host.editor_is_open(i),
                params: host
                    .params(i)
                    .iter()
                    .map(|p| (p.id.0, p.name.clone()))
                    .collect(),
            })
            .collect();

        let table = host.slots();
        self.slots = table
            .slots()
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                let binding = slot.binding.as_ref()?;
                Some((i, binding.param_name.clone(), table.resolved(i).is_some()))
            })
            .collect();
    }
}
