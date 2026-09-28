//! ZoneMinder NG core library. The binary (`main.rs`) is a thin CLI over
//! these modules; integration tests in `tests/` use them directly.
#![recursion_limit = "512"]

pub mod api;
pub mod config;
pub mod db;
pub mod detect;
pub mod health;
pub mod import;
pub mod mp4;
pub mod notify;
pub mod objects;
pub mod preview;
pub mod recorder;
pub mod retention;
pub mod thumbs;
pub mod tier;
pub mod video;
pub mod zmapi;
