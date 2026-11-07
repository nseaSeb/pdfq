//! PDFQ server. Typst never runs in this process: compilations are delegated to
//! supervised `pdfq-worker` processes (see [`pool`]).

pub mod pool;
mod spawn;
