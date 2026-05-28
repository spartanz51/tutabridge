pub(crate) mod bodystructure;
pub(crate) mod parser;
pub(crate) mod rfc2822;

pub use bodystructure::compute_bodystructure;
pub use parser::{Attachment, ParsedMessage};
pub use rfc2822::mail_to_rfc2822;
