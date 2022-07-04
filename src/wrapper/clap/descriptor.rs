use clap_sys::plugin::clap_plugin_descriptor;
use clap_sys::version::CLAP_VERSION;
use std::ffi::{CStr, CString};
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::os::raw::c_char;

use crate::plugin::ClapPlugin;

/// A static descriptor for a plugin. This is used in both the descriptor and on the plugin object
/// itself.
///
/// This cannot be cloned as [`Self::clap_features_ptrs`] contains pointers to
/// [Self::clap_features].
pub struct PluginDescriptor<P: ClapPlugin> {
    // We need [CString]s for all of `ClapPlugin`'s `&str` fields
    clap_id: CString,
    name: CString,
    vendor: CString,
    url: CString,
    version: CString,
    clap_manual_url: Option<CString>,
    clap_support_url: Option<CString>,
    clap_description: Option<CString>,
    clap_features: Vec<CString>,
    clap_features_ptrs: MaybeUninit<Vec<*const c_char>>,

    /// We only support a single plugin per descriptor right now, so we'll fill in the plugin
    /// descriptor upfront. We also need to initialize the `CString` fields above first before we
    /// can initialize this plugin descriptor.
    plugin_descriptor: MaybeUninit<clap_plugin_descriptor>,

    /// The plugin's type.
    _phantom: PhantomData<P>,
}

impl<P: ClapPlugin> Default for PluginDescriptor<P> {
    fn default() -> Self {
        let mut descriptor = Self {
            clap_id: CString::new(P::CLAP_ID).expect("`CLAP_ID` contained null bytes"),
            name: CString::new(P::NAME).expect("`NAME` contained null bytes"),
            vendor: CString::new(P::VENDOR).expect("`VENDOR` contained null bytes"),
            url: CString::new(P::URL).expect("`URL` contained null bytes"),
            version: CString::new(P::VERSION).expect("`VERSION` contained null bytes"),
            clap_manual_url: P::CLAP_MANUAL_URL
                .map(|url| CString::new(url).expect("`CLAP_MANUAL_URL` contained null bytes")),
            clap_support_url: P::CLAP_SUPPORT_URL
                .map(|url| CString::new(url).expect("`CLAP_SUPPORT_URL` contained null bytes")),
            clap_description: P::CLAP_DESCRIPTION.map(|description| {
                CString::new(description).expect("`CLAP_DESCRIPTION` contained null bytes")
            }),
            clap_features: P::CLAP_FEATURES
                .iter()
                .map(|feat| feat.as_str())
                .map(|s| CString::new(s).expect("`CLAP_FEATURES` contained null bytes"))
                .collect(),
            clap_features_ptrs: MaybeUninit::uninit(),

            plugin_descriptor: MaybeUninit::uninit(),

            _phantom: PhantomData,
        };

        // The keyword list is an environ-like list of char pointers terminated by a null pointer.
        let mut clap_features_ptrs: Vec<*const c_char> = descriptor
            .clap_features
            .iter()
            .map(|feature| feature.as_ptr())
            .collect();
        clap_features_ptrs.push(std::ptr::null());
        descriptor.clap_features_ptrs.write(clap_features_ptrs);

        // We couldn't initialize this directly because of all the CStrings
        descriptor.plugin_descriptor.write(clap_plugin_descriptor {
            clap_version: CLAP_VERSION,
            id: descriptor.clap_id.as_ptr(),
            name: descriptor.name.as_ptr(),
            vendor: descriptor.vendor.as_ptr(),
            url: descriptor.url.as_ptr(),
            version: descriptor.version.as_ptr(),
            manual_url: descriptor
                .clap_manual_url
                .as_ref()
                .map(|url| url.as_ptr())
                .unwrap_or(std::ptr::null()),
            support_url: descriptor
                .clap_support_url
                .as_ref()
                .map(|url| url.as_ptr())
                .unwrap_or(std::ptr::null()),
            description: descriptor
                .clap_description
                .as_ref()
                .map(|description| description.as_ptr())
                .unwrap_or(std::ptr::null()),
            features: unsafe { descriptor.clap_features_ptrs.assume_init_ref() }.as_ptr(),
        });

        descriptor
    }
}

unsafe impl<P: ClapPlugin> Send for PluginDescriptor<P> {}
unsafe impl<P: ClapPlugin> Sync for PluginDescriptor<P> {}

impl<P: ClapPlugin> PluginDescriptor<P> {
    pub fn clap_plugin_descriptor(&self) -> &clap_plugin_descriptor {
        unsafe { self.plugin_descriptor.assume_init_ref() }
    }

    pub fn clap_id(&self) -> &CStr {
        self.clap_id.as_c_str()
    }
}
