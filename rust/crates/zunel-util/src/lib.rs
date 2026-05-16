//! Shared zunel helpers (paths, misc).

pub mod http;
pub mod net;
mod paths;
mod text;

pub use http::{read_bytes_capped, read_text_capped, BodyReadError};
pub use net::{default_reqwest_client, filter_blocked_addrs, is_blocked_ip, SsrfSafeResolver};
pub use paths::ensure_dir;
pub use text::truncate_at_char_boundary;
