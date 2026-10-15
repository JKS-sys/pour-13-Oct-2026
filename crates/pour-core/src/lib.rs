//! Pour core: everything the `pour` command does, as a library.
//!
//! Copyright (c) 2026 Jagadeesh Kumar S. All rights reserved. See LICENSE.

pub mod api;
pub mod bottle;
pub mod elf;
pub mod extract;
pub mod index;
pub mod link;
pub mod macho;
pub mod model;
pub mod net;
pub mod paths;
pub mod platform;
pub mod receipt;
pub mod registry;
pub mod relocate;
pub mod store;

pub use index::{Index, IndexBuilder, IndexError};
pub use model::{Cask, Formula, Kind, Pkg};
pub use paths::Paths;
pub use platform::{Arch, Os, Platform};
pub use registry::Registry;
pub mod cask;
pub mod homebrew;
pub mod install;
pub mod linuxsetup;
pub mod lock;
pub mod postinstall;
pub mod resolve;
pub mod selfupdate;
