//! Everything between "the user wants image N" and "decoded pixels are ready": folder listing in
//! natural order, which neighbours to prefetch, and the decoding service with its bounded cache.

pub mod export;
mod folder;
pub mod natord;
mod order;
mod pipeline;

pub use folder::{FolderList, step};
pub use order::prefetch_order;
pub use pipeline::{Pipeline, PipelineConfig, PipelineEvent, Stats};
