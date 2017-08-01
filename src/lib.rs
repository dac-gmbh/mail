#![recursion_limit="128"]
//TODO remove that
extern crate ascii;
extern crate mime;
extern crate owning_ref;
extern crate quoted_printable;
extern crate chrono;
extern crate futures;
extern crate serde;
extern crate base64;
extern crate rand;
//#[macro_use]
//extern crate lazy_static;

#[macro_use]
extern crate serde_derive;

#[macro_use]
extern crate nom;

#[macro_use]
extern crate error_chain;

#[macro_use]
mod macros;
#[cfg_attr(test, macro_use)]
pub mod codec;

pub mod error;
pub mod mail;
pub mod mail_composition;

pub mod headers;
pub mod components;
pub mod grammar;

pub mod types;
mod utils;
