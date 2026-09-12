//! The RAM tools' logic, independent of any emulator or UI: memory regions and address text,
//! typed values and character tables, the RAM search engine and the watch list.

pub mod region;
pub mod table;
pub mod value;

pub use region::*;
pub use table::CharTable;
pub use value::*;
