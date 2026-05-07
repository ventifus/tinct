//! LSP (Language Server Protocol) implementation for LLT.
//!
//! Single-threaded synchronous LSP server using `lsp-server` (not tower-lsp,
//! because `Rc<RefCell<Environment>>` is not Send/Sync).
//!
//! ## Architecture
//!
//! - **Document store**: maintains parsed AST + type errors + eval errors per URI
//! - **Span conversion**: translates between LLT spans (1-indexed, byte offsets)
//!   and LSP positions (0-indexed, UTF-16 code units)
//! - **Analysis**: hover text generation, diagnostics from parse/type/eval errors
//! - **Server loop**: handles LSP requests and notifications, publishes diagnostics

pub mod analysis;
pub(crate) mod convert;
pub mod document;
pub(crate) mod server;

pub use server::run_lsp;
pub(crate) use server::MAX_DOCUMENT_SIZE;
