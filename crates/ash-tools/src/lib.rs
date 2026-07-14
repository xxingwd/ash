pub mod bash;
pub mod edit;
pub mod find;
pub mod grep;
mod path;
pub mod read;
pub mod write;

use ash_core::Tool;
use std::sync::Arc;

pub fn builtin_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        bash::tool(),
        read::tool(),
        write::tool(),
        edit::tool(),
        grep::tool(),
        find::tool(),
    ]
}
