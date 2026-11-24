//! Everything between "the user wants image N" and "decoded pixels are ready": folder listing in
//! the chosen order, which neighbours to prefetch, the folder or archive beside the current one,
//! and the decoding service with its bounded cache.

pub mod archive;
pub mod export;
mod folder;
pub mod natord;
mod order;
mod pipeline;
pub mod siblings;
pub mod thumbnail;

pub use folder::{AtEnd, FolderList, Move, SortBy, SortKey, SortOrder, entry_index, step, step_or_cross};
pub use order::prefetch_order;
pub use pipeline::{FailureKind, Pipeline, PipelineConfig, PipelineEvent, Stats};
