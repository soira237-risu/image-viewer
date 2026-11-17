//! Image edits: the operations, the undoable stack, their reduction to a single [`Geometry`]
//! (for instant GPU preview), and their application to pixels (for saving).

pub mod apply;
mod geometry;
mod op;

pub use geometry::Geometry;
pub use op::{Adjust, EditStack, Op, Rect, ToneTable};
