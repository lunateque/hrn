//! The files a new module crate is born with, as pure text so `module` writes them without
//! touching the disk in its tests.

/// The module crate manifest.
pub(crate) fn cargo_toml(id: &str) -> String {
    format!(
        "[package]\n\
         name = \"hrn-mod-{id}\"\n\
         version.workspace = true\n\
         edition.workspace = true\n\
         rust-version.workspace = true\n\
         license.workspace = true\n\
         publish.workspace = true\n\
         \n\
         [lints]\n\
         workspace = true\n\
         \n\
         [dependencies]\n\
         hrn-kernel.workspace = true\n"
    )
}

pub(crate) fn lib(id: &str) -> String {
    format!(
        "//! The {id} module.\n\
         \n\
         use hrn_kernel::{{Registry, Result}};\n\
         \n\
         /// Register the module's tools, schemes, hooks and config.\n\
         ///\n\
         /// # Errors\n\
         ///\n\
         /// Never fails today; the signature is the module contract.\n\
         pub fn register(_registry: &mut Registry) -> Result {{\n\
        \x20   Ok(())\n\
         }}\n"
    )
}
