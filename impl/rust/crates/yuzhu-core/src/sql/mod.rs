//! Lexing and parsing. Depends only on `error` and `types` (never on the
//! catalog or storage).

pub mod ast;
pub mod lexer;
pub mod parser;
pub mod stack;
pub mod token;

pub use parser::{MAX_NESTING_DEPTH, parse, parse_expr};
pub use stack::{check_stack_depth, set_stack_budget, stack_depth_error};
