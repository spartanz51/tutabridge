pub(crate) mod rfc2822;
pub(crate) mod parser;

pub use rfc2822::mail_to_rfc2822;
pub use parser::{Attachment, ParsedMessage};
