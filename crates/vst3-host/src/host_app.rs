//! The COM objects a plugin expects its host to provide.
//!
//! `IHostApplication` is the context handed to `IPluginBase::initialize`; many
//! plugins refuse to initialise without it, and most use it for exactly one
//! thing — asking the host to allocate an `IMessage` so the processor and the
//! controller can talk to each other.
//!
//! Note the direction of dependency: this module implements the *plumbing*,
//! but the host's identity and policy come in through
//! [`plugin_host_api::HostContext`], which the caller injects (§7). This crate
//! never decides what the host is.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, c_void};
use std::sync::Arc;

use plugin_host_api::HostContext;
use vst3::Steinberg::Vst::{
    IAttributeList, IAttributeList_iid, IAttributeListTrait, IComponentHandler,
    IComponentHandlerTrait, IHostApplication, IHostApplicationTrait, IMessage, IMessage_iid,
    IMessageTrait, ParamID, ParamValue, String128, TChar,
};
use vst3::Steinberg::{
    TUID, char16, int64, kInvalidArgument, kNotImplemented, kResultFalse, kResultOk, kResultTrue,
    tresult, uint32,
};
use vst3::{Class, ComWrapper};

use crate::util::{from_char16, to_char16};

/// Host-side `IHostApplication`.
///
/// Deliberately thin: the only genuine service is `createInstance`, which the
/// SDK's own host classes also implement by hand.
pub struct HostApplication {
    context: Arc<dyn HostContext>,
}

impl HostApplication {
    pub fn new(context: Arc<dyn HostContext>) -> ComWrapper<HostApplication> {
        ComWrapper::new(HostApplication { context })
    }
}

impl Class for HostApplication {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApplication {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kInvalidArgument;
        }
        let dst = unsafe { &mut *(name as *mut [TChar; 128]) };
        to_char16(self.context.host_name(), dst);
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: *mut TUID,
        iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        if cid.is_null() || iid.is_null() || obj.is_null() {
            return kInvalidArgument;
        }
        let cid = unsafe { *cid };
        let iid = unsafe { *iid };

        // Only two classes are ever requested here, and both are pure host-side
        // containers, so implementing them locally is simpler than any form of
        // registry.
        let wants_message = tuid_eq(&cid, &IMessage_iid) || tuid_eq(&iid, &IMessage_iid);
        let wants_attrs = tuid_eq(&cid, &IAttributeList_iid) || tuid_eq(&iid, &IAttributeList_iid);

        if wants_message {
            let wrapper = ComWrapper::new(HostMessage::default());
            if let Some(ptr) = wrapper.to_com_ptr::<IMessage>() {
                unsafe { *obj = ptr.into_raw() as *mut c_void };
                return kResultOk;
            }
        } else if wants_attrs {
            let wrapper = ComWrapper::new(HostAttributeList::default());
            if let Some(ptr) = wrapper.to_com_ptr::<IAttributeList>() {
                unsafe { *obj = ptr.into_raw() as *mut c_void };
                return kResultOk;
            }
        }

        unsafe { *obj = std::ptr::null_mut() };
        kNotImplemented
    }
}

fn tuid_eq(a: &TUID, b: &TUID) -> bool {
    a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

/// One attribute value. VST3 attributes are a tagged union in practice.
#[derive(Clone)]
enum Attr {
    Int(i64),
    Float(f64),
    String(String),
    Binary(Vec<u8>),
}

/// Host-side `IAttributeList`.
///
/// `RefCell` rather than a lock: attribute lists travel with messages, which
/// VST3 confines to the main thread.
#[derive(Default)]
pub struct HostAttributeList {
    attrs: RefCell<HashMap<String, Attr>>,
}

impl Class for HostAttributeList {
    type Interfaces = (IAttributeList,);
}

impl HostAttributeList {
    fn key(id: *const std::ffi::c_char) -> Option<String> {
        if id.is_null() {
            return None;
        }
        unsafe { CStr::from_ptr(id) }
            .to_str()
            .ok()
            .map(|s| s.to_owned())
    }
}

impl IAttributeListTrait for HostAttributeList {
    unsafe fn setInt(&self, id: *const std::ffi::c_char, value: int64) -> tresult {
        match Self::key(id) {
            Some(k) => {
                self.attrs.borrow_mut().insert(k, Attr::Int(value));
                kResultOk
            }
            None => kInvalidArgument,
        }
    }

    unsafe fn getInt(&self, id: *const std::ffi::c_char, value: *mut int64) -> tresult {
        let (Some(k), false) = (Self::key(id), value.is_null()) else {
            return kInvalidArgument;
        };
        match self.attrs.borrow().get(&k) {
            Some(Attr::Int(v)) => {
                unsafe { *value = *v };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setFloat(&self, id: *const std::ffi::c_char, value: f64) -> tresult {
        match Self::key(id) {
            Some(k) => {
                self.attrs.borrow_mut().insert(k, Attr::Float(value));
                kResultOk
            }
            None => kInvalidArgument,
        }
    }

    unsafe fn getFloat(&self, id: *const std::ffi::c_char, value: *mut f64) -> tresult {
        let (Some(k), false) = (Self::key(id), value.is_null()) else {
            return kInvalidArgument;
        };
        match self.attrs.borrow().get(&k) {
            Some(Attr::Float(v)) => {
                unsafe { *value = *v };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setString(&self, id: *const std::ffi::c_char, string: *const TChar) -> tresult {
        let (Some(k), false) = (Self::key(id), string.is_null()) else {
            return kInvalidArgument;
        };
        // The pointer is a NUL-terminated UTF-16 string of unknown length.
        let mut units = Vec::new();
        let mut p = string;
        unsafe {
            while *p != 0 {
                units.push(*p as char16);
                p = p.add(1);
            }
        }
        self.attrs
            .borrow_mut()
            .insert(k, Attr::String(from_char16(&units)));
        kResultOk
    }

    unsafe fn getString(
        &self,
        id: *const std::ffi::c_char,
        string: *mut TChar,
        size_in_bytes: uint32,
    ) -> tresult {
        let (Some(k), false) = (Self::key(id), string.is_null()) else {
            return kInvalidArgument;
        };
        let attrs = self.attrs.borrow();
        let Some(Attr::String(s)) = attrs.get(&k) else {
            return kResultFalse;
        };
        // The size is in *bytes* even though the buffer is UTF-16.
        let capacity = (size_in_bytes as usize) / std::mem::size_of::<TChar>();
        if capacity == 0 {
            return kInvalidArgument;
        }
        let dst = unsafe { std::slice::from_raw_parts_mut(string, capacity) };
        to_char16(s, dst);
        kResultOk
    }

    unsafe fn setBinary(
        &self,
        id: *const std::ffi::c_char,
        data: *const c_void,
        size_in_bytes: uint32,
    ) -> tresult {
        let (Some(k), false) = (Self::key(id), data.is_null()) else {
            return kInvalidArgument;
        };
        let bytes =
            unsafe { std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize) };
        self.attrs
            .borrow_mut()
            .insert(k, Attr::Binary(bytes.to_vec()));
        kResultOk
    }

    unsafe fn getBinary(
        &self,
        id: *const std::ffi::c_char,
        data: *mut *const c_void,
        size_in_bytes: *mut uint32,
    ) -> tresult {
        let (Some(k), false, false) = (Self::key(id), data.is_null(), size_in_bytes.is_null())
        else {
            return kInvalidArgument;
        };
        let attrs = self.attrs.borrow();
        let Some(Attr::Binary(bytes)) = attrs.get(&k) else {
            return kResultFalse;
        };
        // The contract is that the pointer stays valid until the attribute is
        // overwritten or the list dies — hence handing out the stored Vec's
        // buffer rather than a copy.
        unsafe {
            *data = bytes.as_ptr() as *const c_void;
            *size_in_bytes = bytes.len() as uint32;
        }
        kResultOk
    }
}

/// Host-side `IMessage`, the carrier for processor/controller communication.
pub struct HostMessage {
    id: RefCell<Option<std::ffi::CString>>,
    attributes: ComWrapper<HostAttributeList>,
}

impl Default for HostMessage {
    fn default() -> Self {
        HostMessage {
            id: RefCell::new(None),
            attributes: ComWrapper::new(HostAttributeList::default()),
        }
    }
}

impl Class for HostMessage {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for HostMessage {
    unsafe fn getMessageID(&self) -> *const std::ffi::c_char {
        match &*self.id.borrow() {
            Some(s) => s.as_ptr(),
            None => std::ptr::null(),
        }
    }

    unsafe fn setMessageID(&self, id: *const std::ffi::c_char) {
        *self.id.borrow_mut() = if id.is_null() {
            None
        } else {
            Some(unsafe { CStr::from_ptr(id) }.to_owned())
        };
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        // Borrowed, not owned: the caller does not release what `getAttributes`
        // returns, so the message keeps the reference.
        self.attributes
            .as_com_ref::<IAttributeList>()
            .map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
}

/// Host-side `IComponentHandler`: the channel a plugin's own GUI uses to
/// report edits.
///
/// v1 records these and hands them to [`HostContext::param_edited`], which
/// swallows them — in Drive mode the wrapper owns parameter values, so there is
/// nothing to forward upward (§7.5). Keeping the object real anyway matters:
/// plugins that get a null handler often disable their editors.
pub struct ComponentHandler {
    context: Arc<dyn HostContext>,
}

impl ComponentHandler {
    pub fn new(context: Arc<dyn HostContext>) -> ComWrapper<ComponentHandler> {
        ComWrapper::new(ComponentHandler { context })
    }
}

impl Class for ComponentHandler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value_normalized: ParamValue) -> tresult {
        // Normalised here; the caller cannot denormalise without the parameter
        // list, so that translation happens in the plugin wrapper which owns it.
        self.context
            .param_edited(plugin_host_api::ParamId(id), value_normalized);
        kResultOk
    }

    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: i32) -> tresult {
        use plugin_host_api::RestartReason;
        use vst3::Steinberg::Vst::RestartFlags_::{
            kIoChanged, kLatencyChanged, kParamTitlesChanged, kParamValuesChanged,
        };

        // A single call can carry several flags; each is a distinct request.
        let mut handled = false;
        for (flag, reason) in [
            (kParamValuesChanged, RestartReason::ParamValues),
            (kParamTitlesChanged, RestartReason::ParamTitles),
            (kLatencyChanged, RestartReason::Latency),
            (kIoChanged, RestartReason::IoConfig),
        ] {
            if flags & flag != 0 {
                self.context.request_restart(reason);
                handled = true;
            }
        }

        if handled { kResultOk } else { kResultTrue }
    }
}
