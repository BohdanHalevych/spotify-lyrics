//! Bounded YAML/JSON access through the system libfyaml.
use std::{
    ffi::{c_char, c_void},
    marker::PhantomData,
    ptr, slice, str,
};

#[repr(C)]
struct ParseConfig {
    search_path: *const c_char,
    flags: u32,
    userdata: *mut c_void,
    diag: *mut c_void,
}

#[link(name = "fyaml")]
unsafe extern "C" {
    fn fy_document_build_from_string(
        cfg: *const ParseConfig,
        src: *const u8,
        len: usize,
    ) -> *mut c_void;
    fn fy_document_destroy(doc: *mut c_void);
    fn fy_document_root(doc: *mut c_void) -> *mut c_void;
    fn fy_node_mapping_lookup_by_string(
        node: *mut c_void,
        key: *const u8,
        len: usize,
    ) -> *mut c_void;
    fn fy_node_sequence_iterate(node: *mut c_void, cursor: *mut *mut c_void) -> *mut c_void;
    fn fy_node_get_scalar(node: *mut c_void, len: *mut usize) -> *const u8;
    fn fy_node_is_null(node: *mut c_void) -> bool;
}

pub struct Document<'a> {
    raw: *mut c_void,
    _source: PhantomData<&'a str>,
}

#[derive(Clone, Copy)]
pub struct Node<'a>(*mut c_void, PhantomData<&'a ()>);

impl<'a> Document<'a> {
    pub fn parse(text: &'a str, json: bool) -> Option<Self> {
        if text.len() > 2_000_000 {
            return None;
        }
        // Quiet diagnostics, default depth limit, and no alias resolution.
        let cfg = ParseConfig {
            search_path: ptr::null(),
            flags: 1 | if json { 2 << 16 } else { 0 },
            userdata: ptr::null_mut(),
            diag: ptr::null_mut(),
        };
        // The source outlives the document and is read-only.
        let raw = unsafe { fy_document_build_from_string(&cfg, text.as_ptr(), text.len()) };
        (!raw.is_null()).then(|| Self {
            raw,
            _source: PhantomData,
        })
    }

    pub fn root(&self) -> Node<'_> {
        Node(unsafe { fy_document_root(self.raw) }, PhantomData)
    }
}

impl Drop for Document<'_> {
    fn drop(&mut self) {
        unsafe { fy_document_destroy(self.raw) };
    }
}

impl<'a> Node<'a> {
    pub fn get(self, key: &str) -> Self {
        if self.0.is_null() {
            return self;
        }
        Self(
            unsafe { fy_node_mapping_lookup_by_string(self.0, key.as_ptr(), key.len()) },
            PhantomData,
        )
    }

    pub fn text(self) -> Option<&'a str> {
        if self.0.is_null() || unsafe { fy_node_is_null(self.0) } {
            return None;
        }
        let mut len = 0;
        let data = unsafe { fy_node_get_scalar(self.0, &mut len) };
        if data.is_null() {
            return None;
        }
        // The decoded scalar belongs to the document borrowed by this node.
        str::from_utf8(unsafe { slice::from_raw_parts(data, len) }).ok()
    }

    pub fn items(self) -> impl Iterator<Item = Self> {
        let mut cursor = ptr::null_mut();
        std::iter::from_fn(move || {
            if self.0.is_null() {
                return None;
            }
            let raw = unsafe { fy_node_sequence_iterate(self.0, &mut cursor) };
            (!raw.is_null()).then_some(Self(raw, PhantomData))
        })
    }
}
