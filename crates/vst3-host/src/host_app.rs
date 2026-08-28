//! The COM objects a plugin expects its host to provide.
//!
//! `IHostApplication` is the context handed to `IPluginBase::initialize`; many
//! plugins require it to initialize, primarily using it to allocate `IMessage`
//! instances for processor-controller communication.
//!
//! Note the direction of dependency: this module implements the host COM interfaces,
//! while the host's identity and policy are provided by the caller through
//! [`plugin_host_api::HostContext`].

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, c_void};
use std::sync::Arc;

use plugin_host_api::HostContext;
use vst3::Steinberg::Vst::{
    IAttributeList, IAttributeListTrait, IComponentHandler, IComponentHandler2,
    IComponentHandler2Trait, IComponentHandlerTrait, IHostApplication, IHostApplicationTrait,
    IMessage, IMessageTrait, IPlugInterfaceSupport, IPlugInterfaceSupportTrait, ParamID,
    ParamValue, String128, TChar,
};
use vst3::Steinberg::{
    IPlugFrame, TUID, char16, int64, kInvalidArgument, kNotImplemented, kResultFalse, kResultOk,
    kResultTrue, tresult, uint32,
};
use vst3::{Class, ComWrapper, Interface};

use crate::util::{from_char16, to_char16};

/// VST3 spells an interface id as `TUID` (`[c_char; 16]`); the Rust bindings
/// spell the same sixteen bytes as `Guid` (`[u8; 16]`). Only the latter is
/// reachable as a constant, so comparisons go through this.
fn guid(tuid: &TUID) -> [u8; 16] {
    std::array::from_fn(|i| tuid[i] as u8)
}

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
    type Interfaces = (IHostApplication, IPlugInterfaceSupport);
}

impl IPlugInterfaceSupportTrait for HostApplication {
    unsafe fn isPlugInterfaceSupported(&self, iid: *const TUID) -> tresult {
        if iid.is_null() {
            return kInvalidArgument;
        }
        let iid = guid(unsafe { &*iid });
        let supported = iid == IHostApplication::IID
            || iid == IPlugInterfaceSupport::IID
            || iid == IComponentHandler::IID
            || iid == IComponentHandler2::IID
            || iid == IPlugFrame::IID
            || iid == IMessage::IID
            || iid == IAttributeList::IID;
        if supported { kResultOk } else { kResultFalse }
    }
}

impl IHostApplicationTrait for HostApplication {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kInvalidArgument;
        }
        let dst = unsafe { &mut *name };
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

        let (cid, iid) = (guid(&cid), guid(&iid));

        // Only two classes are ever requested here, and both are pure host-side
        // containers, so implementing them locally is simpler than any form of
        // registry.
        let wants_message = cid == IMessage::IID || iid == IMessage::IID;
        let wants_attrs = cid == IAttributeList::IID || iid == IAttributeList::IID;

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

/// Host-side `IComponentHandler`: the interface a plugin GUI uses to report
/// user edits and request host actions.
///
/// Parameter edits are reported through [`HostContext::param_edited`].
/// Providing a concrete component handler is essential, as many plugins
/// disable their UI controls when given a null handler.
pub struct ComponentHandler {
    context: Arc<dyn HostContext>,
}

impl ComponentHandler {
    pub fn new(context: Arc<dyn HostContext>) -> ComWrapper<ComponentHandler> {
        ComWrapper::new(ComponentHandler { context })
    }
}

impl Class for ComponentHandler {
    type Interfaces = (IComponentHandler, IComponentHandler2);
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

impl IComponentHandler2Trait for ComponentHandler {
    unsafe fn setDirty(&self, _state: vst3::Steinberg::TBool) -> tresult {
        kResultOk
    }

    unsafe fn requestOpenEditor(&self, _name: vst3::Steinberg::FIDString) -> tresult {
        kResultOk
    }

    unsafe fn startGroupEdit(&self) -> tresult {
        kResultOk
    }

    unsafe fn finishGroupEdit(&self) -> tresult {
        kResultOk
    }
}
